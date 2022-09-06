pub(crate) mod instruction;

mod error;
mod main_loop;
mod metamethod;
mod opcode;
mod ops;

pub use error::{ErrorKind, Operation, RuntimeError, TracebackFrame};
pub use instruction::Instruction;
pub(crate) use metamethod::Metamethod;
pub use opcode::OpCode;

use crate::{
    gc::{GarbageCollect, GcCell, GcContext, GcHeap, Tracer},
    types::{LuaClosureProto, LuaString, LuaThread, StackWindow, Table, Type, Upvalue, Value},
    LuaClosure,
};

#[derive(Default)]
pub struct Runtime {
    heap: GcHeap,
}

impl Runtime {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn heap(&mut self) -> &mut GcHeap {
        &mut self.heap
    }

    pub fn into_heap(self) -> GcHeap {
        self.heap
    }

    pub fn execute<F>(
        &mut self,
        f: F,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>>
    where
        F: for<'gc> FnOnce(
            &'gc GcContext,
            GcCell<'gc, Vm<'gc>>,
        ) -> Result<
            LuaClosureProto<'gc>,
            Box<dyn std::error::Error + Send + Sync + 'static>,
        >,
    {
        self.heap.with(|gc, vm| {
            let proto = f(gc, vm)?;
            let closure = gc.allocate(proto).into();
            vm.borrow().execute_main(gc, closure)?;
            Ok(())
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Frame {
    bottom: usize,
    base: usize,
    pc: usize,
    num_extra_args: usize,
}

impl Frame {
    fn new(bottom: usize) -> Self {
        Self {
            bottom,
            base: bottom + 1,
            pc: 0,
            num_extra_args: 0,
        }
    }
}

struct ExecutionState<'gc, 'stack> {
    base: usize,
    pc: usize,
    stack: &'stack mut [Value<'gc>],
    lower_stack: &'stack mut [Value<'gc>],
}

impl<'gc, 'stack> ExecutionState<'gc, 'stack> {
    fn resolve_upvalue(&self, upvalue: &Upvalue<'gc>) -> Value<'gc> {
        match upvalue {
            Upvalue::Open(i) => {
                if *i < self.base {
                    self.lower_stack[*i]
                } else {
                    self.stack[*i - self.base]
                }
            }
            Upvalue::Closed(x) => *x,
        }
    }

    fn resolve_upvalue_mut<'a>(&'a mut self, upvalue: &'a mut Upvalue<'gc>) -> &'a mut Value<'gc> {
        match upvalue {
            Upvalue::Open(i) => {
                if *i < self.base {
                    &mut self.lower_stack[*i]
                } else {
                    &mut self.stack[*i - self.base]
                }
            }
            Upvalue::Closed(x) => x,
        }
    }
}

pub struct Vm<'gc> {
    registry: GcCell<'gc, Table<'gc>>,
    main_thread: GcCell<'gc, LuaThread<'gc>>,
    globals: GcCell<'gc, Table<'gc>>,
    metamethod_names: [LuaString<'gc>; Metamethod::COUNT],
    metatables: GcCell<'gc, [Option<GcCell<'gc, Table<'gc>>>; Type::COUNT]>,
}

unsafe impl GarbageCollect for Vm<'_> {
    fn trace(&self, tracer: &mut Tracer) {
        self.registry.trace(tracer);
        self.main_thread.trace(tracer);
        self.globals.trace(tracer);
        self.metamethod_names.trace(tracer);
        self.metatables.trace(tracer);
    }
}

impl<'gc> Vm<'gc> {
    pub(crate) fn new(gc: &'gc GcContext) -> Self {
        let main_thread = gc.allocate_cell(LuaThread::new());
        let globals = gc.allocate_cell(Table::new());
        let registry = Table::from(vec![main_thread.into(), globals.into()]);
        Self {
            registry: gc.allocate_cell(registry),
            main_thread,
            globals,
            metamethod_names: Metamethod::allocate_names(gc),
            metatables: gc.allocate_cell(Default::default()),
        }
    }

    pub fn registry(&self) -> GcCell<'gc, Table<'gc>> {
        self.registry
    }

    pub fn globals(&self) -> GcCell<'gc, Table<'gc>> {
        self.globals
    }

    pub fn load_stdlib(&self, gc: &'gc GcContext) {
        crate::stdlib::load(gc, self);
    }

    pub fn metamethod_name(&self, metamethod: Metamethod) -> LuaString<'gc> {
        self.metamethod_names[metamethod as usize]
    }

    pub fn metatable_of_object(&self, object: Value<'gc>) -> Option<GcCell<'gc, Table<'gc>>> {
        object
            .metatable()
            .or_else(|| self.metatables.borrow()[object.ty() as usize])
    }

    pub fn set_metatable_of_type<T>(&self, gc: &'gc GcContext, ty: Type, metatable: T)
    where
        T: Into<Option<GcCell<'gc, Table<'gc>>>>,
    {
        self.metatables.borrow_mut(gc)[ty as usize] = metatable.into();
    }

    fn execute_main(
        &self,
        gc: &'gc GcContext,
        mut closure: LuaClosure<'gc>,
    ) -> Result<(), RuntimeError> {
        assert!(closure.upvalues.is_empty());
        closure
            .upvalues
            .push(gc.allocate_cell(Value::Table(self.globals).into()));

        {
            let thread = self.main_thread.borrow();
            assert!(thread.stack.is_empty());
            assert!(thread.frames.is_empty());
            assert!(thread.open_upvalues.is_empty());
        }

        let result =
            unsafe { self.execute_value(gc, self.main_thread, gc.allocate(closure).into(), &[]) };
        match result {
            Ok(_) => Ok(()),
            Err(kind) => {
                let mut thread = self.main_thread.borrow_mut(gc);
                let frames = std::mem::take(&mut thread.frames);
                let traceback = frames
                    .into_iter()
                    .rev()
                    .map(|frame| {
                        let value = thread.stack[frame.bottom];
                        let proto = value.as_lua_closure().unwrap().proto;
                        TracebackFrame {
                            source: String::from_utf8_lossy(&proto.source).to_string(),
                            lines_defined: proto.lines_defined.clone(),
                        }
                    })
                    .collect();
                thread.stack.clear();
                thread.open_upvalues.clear();
                Err(RuntimeError { kind, traceback })
            }
        }
    }

    pub(crate) unsafe fn execute_value(
        &self,
        gc: &'gc GcContext,
        thread: GcCell<'gc, LuaThread<'gc>>,
        callee: Value<'gc>,
        args: &[Value<'gc>],
    ) -> Result<Value<'gc>, ErrorKind> {
        let mut thread_ref = thread.borrow_mut(gc);
        let bottom = thread_ref.stack.len();
        thread_ref.stack.reserve(args.len() + 1);
        thread_ref.stack.push(callee);
        thread_ref.stack.extend_from_slice(args);

        let frame_level = thread_ref.frames.len();
        thread_ref.frames.push(Frame::new(bottom));
        drop(thread_ref);

        loop {
            self.execute_frame(gc, thread)?;
            if thread.borrow().frames.len() <= frame_level {
                break;
            }
            if gc.debt() > 0 {
                gc.step_unguarded();
            }
        }

        thread.borrow_mut(gc).stack.truncate(bottom + 1);
        if gc.debt() > 0 {
            gc.step_unguarded();
        }

        let result = thread
            .borrow_mut(gc)
            .stack
            .drain(bottom..)
            .next()
            .unwrap_or_default();
        Ok(result)
    }

    fn call_value(
        &self,
        gc: &'gc GcContext,
        thread: GcCell<'gc, LuaThread<'gc>>,
        callee: Value,
        bottom: usize,
    ) -> Result<(), ErrorKind> {
        match callee {
            Value::LuaClosure(_) => thread.borrow_mut(gc).frames.push(Frame::new(bottom)),
            Value::NativeFunction(func) => {
                let num_results = func.0(gc, self, thread, StackWindow { bottom })?;
                thread.borrow_mut(gc).stack.truncate(bottom + num_results);
            }
            Value::NativeClosure(closure) => {
                let num_results = (closure.function())(gc, self, thread, StackWindow { bottom })?;
                thread.borrow_mut(gc).stack.truncate(bottom + num_results);
            }
            value => {
                return Err(ErrorKind::TypeError {
                    operation: Operation::Call,
                    ty: value.ty(),
                })
            }
        }
        Ok(())
    }

    unsafe fn call_index_metamethod<K>(
        &self,
        gc: &'gc GcContext,
        thread: GcCell<'gc, LuaThread<'gc>>,
        mut table_like: Value<'gc>,
        key: K,
    ) -> Result<Value<'gc>, ErrorKind>
    where
        K: Into<Value<'gc>>,
    {
        let key = key.into();
        let index_key = self.metamethod_name(Metamethod::Index);
        loop {
            let metamethod = if let Value::Table(table) = table_like {
                let metamethod = table
                    .borrow()
                    .metatable()
                    .map(|metatable| metatable.borrow().get_field(index_key))
                    .unwrap_or_default();
                if metamethod.is_nil() {
                    return Ok(Value::Nil);
                }
                metamethod
            } else {
                let metamethod = self
                    .metatable_of_object(table_like)
                    .map(|metatable| metatable.borrow().get_field(index_key))
                    .unwrap_or_default();
                if metamethod.is_nil() {
                    return Err(ErrorKind::TypeError {
                        operation: Operation::Index,
                        ty: table_like.ty(),
                    });
                }
                metamethod
            };
            match metamethod {
                Value::NativeFunction(_) | Value::LuaClosure(_) | Value::NativeClosure(_) => {
                    return self.execute_value(gc, thread, metamethod, &[table_like, key])
                }
                Value::Table(table) => {
                    let value = table.borrow().get(key);
                    if !value.is_nil() {
                        return Ok(value);
                    }
                }
                Value::Nil => unreachable!(),
                _ => (),
            }
            table_like = metamethod;
        }
    }
}
