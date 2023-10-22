mod token;

pub use token::Token;

use crate::{gc::GcContext, string};
use std::{
    collections::VecDeque,
    io::{Bytes, Read},
};

#[derive(Debug, thiserror::Error)]
pub enum LexerError {
    #[error("unknown token \"{}\"", char::from(*.0))]
    UnknownToken(u8),

    #[error("invalid escape sequence \"{}\"", char::from(*.0))]
    InvalidEscapeSequence(u8),

    #[error("decimal escape too large")]
    DecimalEscapeTooLarge,

    #[error("invalid long string delimiter")]
    InvalidLongStringDelimiter,

    #[error("unfinished {0}")]
    UnfinishedToken(&'static str),

    #[error("malformed number")]
    MalformedNumber,

    #[error("hexadecimal digit expected")]
    HexadecimalDigitExpected,

    #[error("missing '{}'", char::from(*.0))]
    MissingEscapeCharacter(u8),

    #[error("UTF-8 value too large")]
    Utf8ValueTooLarge,

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub struct Lexer<'gc, R: Read> {
    inner: LexerInner<'gc, R>,
    peeked: VecDeque<Token<'gc>>,
}

impl<'gc, R: Read> Lexer<'gc, R> {
    pub fn new(gc: &'gc GcContext, reader: R) -> Self {
        Self {
            inner: LexerInner::new(gc, reader),
            peeked: VecDeque::with_capacity(2),
        }
    }

    pub fn consume(&mut self) -> Result<Option<Token<'gc>>, LexerError> {
        if let Some(peeked) = self.peeked.pop_front() {
            Ok(Some(peeked))
        } else {
            self.inner.consume_token()
        }
    }

    pub fn consume_if(
        &mut self,
        func: impl Fn(&Token) -> bool,
    ) -> Result<Option<Token>, LexerError> {
        if let Some(token) = self.peek()? {
            if func(token) {
                return self.consume();
            }
        }
        Ok(None)
    }

    pub fn consume_if_eq(&mut self, expected: Token) -> Result<bool, LexerError> {
        Ok(self.consume_if(|token| *token == expected)?.is_some())
    }

    pub fn peek(&mut self) -> Result<Option<&Token<'gc>>, LexerError> {
        if self.peeked.is_empty() {
            if let Some(token) = self.inner.consume_token()? {
                self.peeked.push_back(token);
            }
        }
        Ok(self.peeked.front())
    }

    pub fn peek2(&mut self) -> Result<Option<&Token>, LexerError> {
        if self.peeked.len() < 2 {
            if let Some(token) = self.inner.consume_token()? {
                self.peeked.push_back(token);
            }
        }
        Ok(self.peeked.get(1))
    }

    pub const fn lineno(&self) -> usize {
        self.inner.lineno
    }
}

struct LexerInner<'gc, R: Read> {
    gc: &'gc GcContext,
    bytes: Bytes<R>,
    peeked: VecDeque<u8>,
    lineno: usize,
}

impl<'gc, R: Read> LexerInner<'gc, R> {
    fn new(gc: &'gc GcContext, reader: R) -> LexerInner<'gc, R> {
        Self {
            gc,
            bytes: reader.bytes(),
            peeked: Default::default(),
            lineno: 1,
        }
    }

    fn consume_token(&mut self) -> Result<Option<Token<'gc>>, LexerError> {
        while let Some(ch) = self.peek()? {
            match ch {
                b'\n' | b'\r' => self.consume_newline()?,
                b' ' | 0xc | b'\t' | 0xb => {
                    self.consume()?;
                }
                b'-' => {
                    self.consume()?;
                    if !self.consume_if_eq(b'-')? {
                        return Ok(Some(Token::Minus));
                    }
                    if self.consume_long_comment()? {
                        continue;
                    }
                    while self.consume_if(|ch| !is_newline(ch))?.is_some() {}
                }
                b'[' => {
                    self.consume()?;
                    let mut opening_level = 0;
                    while self.consume_if_eq(b'=')? {
                        opening_level += 1;
                    }
                    return if self.consume_if_eq(b'[')? {
                        self.consume_long_string(opening_level).map(Some)
                    } else if opening_level == 0 {
                        Ok(Some(Token::LeftBracket))
                    } else {
                        Err(LexerError::InvalidLongStringDelimiter)
                    };
                }
                b'=' => {
                    self.consume()?;
                    return Ok(Some(if self.consume_if_eq(b'=')? {
                        Token::Eq
                    } else {
                        Token::Assign
                    }));
                }
                b'<' => {
                    self.consume()?;
                    return Ok(Some(if self.consume_if_eq(b'=')? {
                        Token::Le
                    } else if self.consume_if_eq(b'<')? {
                        Token::Shl
                    } else {
                        Token::Lt
                    }));
                }
                b'>' => {
                    self.consume()?;
                    return Ok(Some(if self.consume_if_eq(b'=')? {
                        Token::Ge
                    } else if self.consume_if_eq(b'>')? {
                        Token::Shr
                    } else {
                        Token::Gt
                    }));
                }
                b'/' => {
                    self.consume()?;
                    return Ok(Some(if self.consume_if_eq(b'/')? {
                        Token::IDiv
                    } else {
                        Token::Div
                    }));
                }
                b'~' => {
                    self.consume()?;
                    return Ok(Some(if self.consume_if_eq(b'=')? {
                        Token::Ne
                    } else {
                        Token::Tilde
                    }));
                }
                b':' => {
                    self.consume()?;
                    return Ok(Some(if self.consume_if_eq(b':')? {
                        Token::DoubleColon
                    } else {
                        Token::Colon
                    }));
                }
                b'"' | b'\'' => return self.consume_string().map(Some),
                b'.' => {
                    return match self.peek2()? {
                        Some(b'.') => {
                            self.consume()?;
                            self.consume()?;
                            Ok(Some(if self.consume_if_eq(b'.')? {
                                Token::Dots
                            } else {
                                Token::Concat
                            }))
                        }
                        Some(ch) if ch.is_ascii_digit() => self.consume_numeral().map(Some),
                        _ => {
                            self.consume()?;
                            Ok(Some(Token::Dot))
                        }
                    };
                }
                b'0'..=b'9' => return self.consume_numeral().map(Some),
                _ if is_lua_alphabetic(ch) => {
                    let mut string = Vec::new();
                    self.consume_while(is_lua_alphanumeric, &mut string)?;
                    return Ok(Some(
                        Token::from_reserved_word(&string)
                            .unwrap_or_else(|| Token::Name(self.gc.allocate_string(string))),
                    ));
                }
                ch => {
                    self.consume()?;
                    return Ok(Some(match ch {
                        b'#' => Token::Len,
                        b'%' => Token::Mod,
                        b'&' => Token::BAnd,
                        b'(' => Token::LeftParen,
                        b')' => Token::RightParen,
                        b'*' => Token::Mul,
                        b'+' => Token::Add,
                        b',' => Token::Comma,
                        b';' => Token::Semicolon,
                        b']' => Token::RightBracket,
                        b'^' => Token::Pow,
                        b'{' => Token::LeftCurlyBracket,
                        b'|' => Token::BOr,
                        b'}' => Token::RightCurlyBracket,
                        _ => return Err(LexerError::UnknownToken(ch)),
                    }));
                }
            }
        }
        Ok(None)
    }

    fn consume_newline(&mut self) -> std::io::Result<()> {
        let ch = self.consume_if(is_newline)?.unwrap();
        self.consume_if(|next| is_newline(next) && next != ch)?;
        self.lineno += 1;
        Ok(())
    }

    fn consume_numeral(&mut self) -> Result<Token<'gc>, LexerError> {
        let first_ch = self.consume()?.unwrap();
        let mut bytes = Vec::new();
        let is_hex = if first_ch == b'0'
            && self
                .consume_if(|ch| ch.eq_ignore_ascii_case(&b'x'))?
                .is_some()
        {
            true
        } else {
            bytes.push(first_ch);
            false
        };
        let exp_ch = if is_hex { b'p' } else { b'e' };
        loop {
            if let Some(ch) = self.consume_if(|ch| ch.eq_ignore_ascii_case(&exp_ch))? {
                bytes.push(ch);
                if let Some(ch) = self.consume_if(|ch| ch == b'-' || ch == b'+')? {
                    bytes.push(ch);
                }
            } else if let Some(ch) = self.consume_if(|ch| ch.is_ascii_hexdigit() || ch == b'.')? {
                bytes.push(ch);
            } else {
                break;
            }
        }
        if let Some(ch) = self.consume_if(is_lua_alphabetic)? {
            bytes.push(ch);
        }
        if is_hex {
            if let Some(i) = string::parse_positive_integer_with_base(&bytes, 16) {
                return Ok(Token::Integer(i));
            }
            if let Some(x) = string::parse_positive_hex_float(&bytes) {
                return Ok(Token::Float(x));
            }
        } else {
            let string = String::from_utf8(bytes).map_err(|_| LexerError::MalformedNumber)?;
            if let Ok(i) = string.parse() {
                return Ok(Token::Integer(i));
            }
            if let Ok(x) = string.parse() {
                return Ok(Token::Float(x));
            }
        }
        Err(LexerError::MalformedNumber)
    }

    fn consume_string(&mut self) -> Result<Token<'gc>, LexerError> {
        let delimiter = self.consume_if(|ch| ch == b'"' || ch == b'\'')?.unwrap();
        let mut string = Vec::new();
        while let Some(ch) = self.consume()? {
            match ch {
                b'\n' | b'\r' => break,
                b'\\' => match self.peek()? {
                    None => break,
                    Some(b'a') => {
                        self.consume()?;
                        string.push(0x7);
                    }
                    Some(b'b') => {
                        self.consume()?;
                        string.push(0x8);
                    }
                    Some(b'f') => {
                        self.consume()?;
                        string.push(0xc);
                    }
                    Some(b'n') => {
                        self.consume()?;
                        string.push(b'\n');
                    }
                    Some(b'r') => {
                        self.consume()?;
                        string.push(b'\r');
                    }
                    Some(b't') => {
                        self.consume()?;
                        string.push(b'\t');
                    }
                    Some(b'v') => {
                        self.consume()?;
                        string.push(0xb);
                    }
                    Some(b'x') => {
                        self.consume()?;
                        string.push(self.consume_hex_escape()?);
                    }
                    Some(b'u') => {
                        self.consume()?;
                        self.consume_utf8_escape(&mut string)?;
                    }
                    Some(b'\n' | b'\r') => {
                        self.consume_newline()?;
                        string.push(b'\n');
                    }
                    Some(ch @ (b'\\' | b'\"' | b'\'')) => {
                        self.consume()?;
                        string.push(ch);
                    }
                    Some(b'z') => {
                        self.consume()?;
                        self.consume_zap()?;
                    }
                    Some(ch) => {
                        if ch.is_ascii_digit() {
                            string.push(self.consume_decimal_escape()?);
                        } else {
                            return Err(LexerError::InvalidEscapeSequence(ch));
                        }
                    }
                },
                _ if ch == delimiter => return Ok(Token::String(self.gc.allocate_string(string))),
                _ => string.push(ch),
            }
        }
        Err(LexerError::UnfinishedToken("string"))
    }

    fn consume_decimal_escape(&mut self) -> Result<u8, LexerError> {
        let mut r = 0;
        for _ in 0..3 {
            if let Some(ch) = self.consume_if(|ch| ch.is_ascii_digit())? {
                r = 10 * r + (ch - b'0') as usize;
            } else {
                break;
            }
        }
        if let Ok(i) = r.try_into() {
            Ok(i)
        } else {
            Err(LexerError::DecimalEscapeTooLarge)
        }
    }

    fn consume_hex_escape(&mut self) -> Result<u8, LexerError> {
        let a = self.consume_if(|ch| ch.is_ascii_hexdigit())?;
        let b = self.consume_if(|ch| ch.is_ascii_hexdigit())?;
        match (a, b) {
            (Some(a), Some(b)) => {
                Ok(string::parse_hex_digit(a).unwrap() * 16 + string::parse_hex_digit(b).unwrap())
            }
            _ => Err(LexerError::HexadecimalDigitExpected),
        }
    }

    fn consume_utf8_escape(&mut self, buf: &mut Vec<u8>) -> Result<(), LexerError> {
        if !self.consume_if_eq(b'{')? {
            return Err(LexerError::MissingEscapeCharacter(b'{'));
        }

        let mut code = match self.consume_if(|ch| ch.is_ascii_hexdigit())? {
            Some(ch) => string::parse_hex_digit(ch).unwrap() as u32,
            None => return Err(LexerError::HexadecimalDigitExpected),
        };
        while let Some(ch) = self.consume_if(|ch| ch.is_ascii_hexdigit())? {
            if code > (string::MAX_UTF8 / 16) {
                return Err(LexerError::Utf8ValueTooLarge);
            }
            code = code * 16 + string::parse_hex_digit(ch).unwrap() as u32;
        }

        if !self.consume_if_eq(b'}')? {
            return Err(LexerError::MissingEscapeCharacter(b'}'));
        }

        assert!(string::encode_utf8(code, buf));
        Ok(())
    }

    fn consume_zap(&mut self) -> std::io::Result<()> {
        loop {
            match self.peek()? {
                Some(ch) if is_newline(ch) => self.consume_newline()?,
                Some(ch) if string::is_lua_whitespace(ch) => {
                    self.consume()?;
                }
                _ => return Ok(()),
            }
        }
    }

    fn consume_long_string(&mut self, opening_level: usize) -> Result<Token<'gc>, LexerError> {
        match self.peek()? {
            Some(ch) if is_newline(ch) => self.consume_newline()?,
            _ => (),
        }
        let mut buf = Vec::new();
        while let Some(ch) = self.peek()? {
            match ch {
                b']' => {
                    self.consume()?;
                    loop {
                        let mut closing_level = 0;
                        while self.consume_if_eq(b'=')? {
                            closing_level += 1;
                        }
                        let has_second_bracket = self.consume_if_eq(b']')?;
                        if has_second_bracket && closing_level == opening_level {
                            return Ok(Token::String(self.gc.allocate_string(buf)));
                        }
                        buf.push(b']');
                        buf.resize(buf.len() + closing_level, b'=');
                        if !has_second_bracket {
                            break;
                        }
                    }
                }
                b'\n' | b'\r' => {
                    self.consume_newline()?;
                    buf.push(b'\n');
                }
                _ => {
                    self.consume()?;
                    buf.push(ch)
                }
            }
        }
        Err(LexerError::UnfinishedToken("long string"))
    }

    fn consume_long_comment(&mut self) -> Result<bool, LexerError> {
        if !self.consume_if_eq(b'[')? {
            return Ok(false);
        }
        let mut opening_level = 0;
        while self.consume_if_eq(b'=')? {
            opening_level += 1;
        }
        if !self.consume_if_eq(b'[')? {
            return Ok(false);
        }
        while let Some(ch) = self.peek()? {
            match ch {
                b']' => {
                    self.consume()?;
                    loop {
                        let mut closing_level = 0;
                        while self.consume_if_eq(b'=')? {
                            closing_level += 1;
                        }
                        if !self.consume_if_eq(b']')? {
                            break;
                        }
                        if closing_level == opening_level {
                            return Ok(true);
                        }
                    }
                }
                b'\n' | b'\r' => self.consume_newline()?,
                _ => {
                    self.consume()?;
                }
            }
        }
        Err(LexerError::UnfinishedToken("long comment"))
    }

    fn peek(&mut self) -> std::io::Result<Option<u8>> {
        if self.peeked.is_empty() {
            if let Some(ch) = self.bytes.next().transpose()? {
                self.peeked.push_back(ch);
            }
        }
        Ok(self.peeked.front().copied())
    }

    fn peek2(&mut self) -> std::io::Result<Option<u8>> {
        if self.peeked.len() < 2 {
            if let Some(ch) = self.bytes.next().transpose()? {
                self.peeked.push_back(ch);
            }
        }
        Ok(self.peeked.get(1).copied())
    }

    fn consume(&mut self) -> std::io::Result<Option<u8>> {
        if let Some(peeked) = self.peeked.pop_front() {
            Ok(Some(peeked))
        } else {
            self.bytes.next().transpose()
        }
    }

    fn consume_if(&mut self, func: impl Fn(u8) -> bool) -> std::io::Result<Option<u8>> {
        if let Some(ch) = self.peek()? {
            if func(ch) {
                return self.consume();
            }
        }
        Ok(None)
    }

    fn consume_if_eq(&mut self, expected: u8) -> std::io::Result<bool> {
        Ok(self.consume_if(|ch| ch == expected)?.is_some())
    }

    fn consume_while(
        &mut self,
        func: impl Fn(u8) -> bool,
        buf: &mut Vec<u8>,
    ) -> std::io::Result<()> {
        while let Some(ch) = self.consume_if(&func)? {
            buf.push(ch);
        }
        Ok(())
    }
}

const fn is_newline(ch: u8) -> bool {
    ch == b'\n' || ch == b'\r'
}

const fn is_lua_alphabetic(ch: u8) -> bool {
    ch.is_ascii_alphabetic() || ch == b'_'
}

const fn is_lua_alphanumeric(ch: u8) -> bool {
    ch.is_ascii_alphanumeric() || ch == b'_'
}
