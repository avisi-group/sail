//! JIB contains bitvectors GenC doesn't, need to represent them somehow.
//!
//! Inspection of all values of bitvector lengths in the ARMv8 model shows:
//!
//! * Minimum: 1
//! * Maximum: 64
//! * Mean: 2.88
//! * Mode: 1
//!
//! So almost always a tiny value, and all fit in a `uint64`, but length must be
//! stored too somehow. Possibly in the high 64 bits of a uint128?
//!
//! Length is known at compile time, and eq_vec is a builtin function, so could
//! pass in length values then?
//!
//! Maybe length doesn't matter, initially trying to replace them all with
//! uint64s
//!
//! new plan: try to determine the lenght of every non fixed bv at compile time
//!
//! Some bv's might be impossible (e.g. imm length is runtime dependant) but
//! hoping as long as these don't involve concatenation we don't need to know
//! the length

use {
    crate::{
        boom::{
            bits_to_int,
            visitor::{Visitor, Walkable},
            Ast, Expression, FunctionDefinition, Literal, Operation, Parameter, Size, Statement,
            Type, Value,
        },
        passes::{any::AnyExt, Pass},
    },
    common::{intern::InternedString, HashMap},
    num_bigint::BigInt,
    once_cell::sync::Lazy,
    std::{cell::RefCell, rc::Rc},
};

#[derive(Debug)]
pub struct ResolveBitvectors {
    did_change: bool,
    /// local variables and their types (containing bitvector length
    /// information)
    locals: HashMap<InternedString, Rc<RefCell<Type>>>,
    current_func: Option<FunctionDefinition>,
}

impl Pass for ResolveBitvectors {
    fn name(&self) -> &'static str {
        "ReplaceBitvectors"
    }

    fn reset(&mut self) {
        self.did_change = false;
        self.locals.clear();
        self.current_func = None;
    }

    fn run(&mut self, ast: Rc<RefCell<Ast>>) -> bool {
        ast.borrow()
            .functions
            .values()
            .map(|func| {
                self.reset();

                self.locals
                    .extend(func.signature.parameters.borrow().iter().filter_map(
                        |Parameter { name, typ, .. }| {
                            if let Type::Int { .. } = &*typ.borrow() {
                                Some((*name, typ.clone()))
                            } else {
                                None
                            }
                        },
                    ));

                self.visit_function_definition(func);

                self.did_change
            })
            .any()
    }
}

impl Visitor for ResolveBitvectors {
    fn visit_function_definition(&mut self, node: &FunctionDefinition) {
        self.current_func = Some(node.clone());
        node.walk(self);
    }

    fn visit_statement(&mut self, node: Rc<RefCell<Statement>>) {
        let statement = { node.borrow().clone() };
        match statement {
            Statement::TypeDeclaration { name, typ } => {
                self.add_type_declaration(name, typ.clone())
            }
            Statement::Copy { expression, value } => {
                self.resolve_from_copy(&expression, value.clone())
            }
            Statement::FunctionCall {
                expression: Some(expression),
                name,
                arguments,
            } => self.resolve_fn(node.clone(), &expression, name, &arguments),
            _ => {}
        }

        // use assignments to determine length
        // function calls are special:/ need to think about handling these
    }
}

impl ResolveBitvectors {
    pub fn new_boxed() -> Box<dyn Pass> {
        Box::new(Self {
            did_change: false,
            locals: HashMap::default(),
            current_func: None,
        })
    }

    /// Gets the size of a local variable, None if not an int or a local
    /// variable
    fn get_size(&self, name: InternedString) -> Option<Size> {
        self.locals
            .get(&name)
            .map(|t| t.borrow().get_size())
            .flatten()
    }

    /// Sets the size of a local variable
    fn set_size(&self, name: InternedString, size: Size) {
        let typ = self.locals.get(&name).unwrap();
        *typ.borrow_mut().get_size_mut().unwrap() = size;
    }

    /// Adds a bitvector type declaration to the mapping
    fn add_type_declaration(&mut self, name: InternedString, typ: Rc<RefCell<Type>>) {
        self.locals.insert(name, typ);
    }

    /// Try to use the value being assigned to a bitvector to determine it's
    /// length
    fn resolve_from_copy(&mut self, expression: &Expression, value: Rc<RefCell<Value>>) {
        let Expression::Identifier(dest) = expression else {
            return;
        };

        // if the identifier being copied into is a variable bitvector, try and resolve
        // it's length

        match &*value.borrow() {
            Value::Identifier(source) => {
                // set the dest size to be the source size
                // and that identifier has a known length
                match (self.get_size(*dest), self.get_size(*source)) {
                    // do not override destination if already static
                    // TODO: make sure this is always the best heuristic (shortest/longest length?
                    // oldest/newest assignment?)
                    (Some(Size::Static(_)), Some(_)) => (),

                    // if destination is unknown, replace with source
                    (Some(Size::Unknown), Some(source_size)) => {
                        self.set_size(*dest, source_size);
                    }

                    // if destination is runtime and source is static, assign source size
                    (Some(Size::Runtime(_)), Some(Size::Static(size))) => {
                        self.set_size(*dest, Size::Static(size));
                    }

                    // otherwise do nothing
                    _ => (),
                }
            }

            Value::Literal(literal) => {
                let literal = &mut *literal.borrow_mut();
                if let Literal::Bits(bits) = literal {
                    // set size as static
                    self.set_size(*dest, Size::Static(bits.len()));

                    // replace bits with constant int
                    *literal = Literal::Int(BigInt::from(bits_to_int(bits)));
                }
            }

            _ => (),
        }
    }

    /// Resolves bitvectors in a function call
    ///
    /// If the function is a builtin bitvector operation (`eq_vec`, `Zeros`,
    /// etc), replace it with the corresponding logic.
    ///
    /// If the function has variable bitvector parameters, use arguments
    /// supplied to a function call to generate a monomorphised version of that
    /// function with fixed bitvector paramaters.
    fn resolve_fn(
        &mut self,
        statement: Rc<RefCell<Statement>>,
        expression: &Expression,
        name: InternedString,
        arguments: &[Rc<RefCell<Value>>],
    ) {
        type HandlerFunction =
            fn(&mut ResolveBitvectors, Rc<RefCell<Statement>>, &Expression, &[Rc<RefCell<Value>>]);

        // function handlers
        static HANDLERS: Lazy<HashMap<InternedString, HandlerFunction>> = Lazy::new(|| {
            let mappings = [
                ("Zeros", zeros_handler as HandlerFunction),
                ("Ones", ones_handler),
                ("bitvector_concat", concat_handler),
                ("eq_vec", eq_handler),
                ("undefined_bitvector", undefined_handler),
            ]
            .into_iter()
            .map(|(s, f)| (InternedString::from_static(s), f));

            HashMap::from_iter(mappings)
        });

        // execute function handler if the function call is to a builtin bitvector
        // function
        if let Some(handler) = HANDLERS.get(&name) {
            handler(self, statement.clone(), expression, arguments);
        }
    }
}

fn zeros_handler(
    celf: &mut ResolveBitvectors,
    statement: Rc<RefCell<Statement>>,
    expression: &Expression,
    arguments: &[Rc<RefCell<Value>>],
) {
    // get assignment to argument to Zeros
    assert_eq!(arguments.len(), 1);

    let Value::Identifier(ident) = &*arguments[0].borrow() else {
        panic!();
    };

    // resolve destination length if possible
    if let Some(value) = celf
        .current_func
        .as_ref()
        .unwrap()
        .entry_block
        .get_assignment(*ident)
    {
        if let Value::Literal(literal) = &*value.borrow() {
            if let Literal::Int(length) = &*literal.borrow() {
                if let Expression::Identifier(destination) = expression {
                    celf.set_size(*destination, Size::Static(length.try_into().unwrap()));
                }
            }
        }
    }

    // assign literal 0
    *statement.borrow_mut() = Statement::Copy {
        expression: expression.clone(),
        value: Literal::Int(0.into()).into(),
    }
}

fn ones_handler(
    celf: &mut ResolveBitvectors,
    statement: Rc<RefCell<Statement>>,
    expression: &Expression,
    arguments: &[Rc<RefCell<Value>>],
) {
    // get assignment to argument to Ones
    assert_eq!(arguments.len(), 1);

    let Value::Identifier(ident) = &*arguments[0].borrow() else {
        panic!();
    };

    let Some(value) = celf
        .current_func
        .as_ref()
        .unwrap()
        .entry_block
        .get_assignment(*ident)
    else {
        return;
    };

    let Value::Literal(literal) = &*value.borrow() else {
        panic!();
    };

    let Literal::Int(length) = &*literal.borrow() else {
        panic!();
    };

    // change type of destination to length
    let Expression::Identifier(destination) = expression else {
        panic!();
    };

    celf.set_size(*destination, Size::Static(length.try_into().unwrap()));

    // assign all 1s
    *statement.borrow_mut() = Statement::Copy {
        expression: expression.clone(),
        value: Literal::Int(((1u128 << u64::try_from(length).unwrap()) - 1).into()).into(),
    }
}

fn concat_handler(
    celf: &mut ResolveBitvectors,
    statement: Rc<RefCell<Statement>>,
    expression: &Expression,
    arguments: &[Rc<RefCell<Value>>],
) {
    // get identifiers and lengths of input bitvectors
    assert_eq!(arguments.len(), 2);

    let Value::Identifier(left_ident) = &*arguments[0].borrow() else {
        panic!();
    };

    let Value::Identifier(right_ident) = &*arguments[1].borrow() else {
        panic!();
    };

    let Some(Size::Static(left_length)) = celf.get_size(*left_ident) else {
        panic!(
            "{left_ident} not static, got {:?}\n {:#?}",
            celf.get_size(*left_ident),
            celf
        );
    };

    let Some(Size::Static(right_length)) = celf.get_size(*right_ident) else {
        panic!(
            "{right_ident} not static, got {:?}\n {:#?}",
            celf.get_size(*right_ident),
            celf
        );
    };

    // generate shifting and & logic
    // (left << right_length) | right
    let value = Operation::Or(
        Operation::LeftShift(
            Rc::new(RefCell::new(Value::Identifier(*left_ident))),
            Literal::Int(right_length.into()).into(),
        )
        .into(),
        Rc::new(RefCell::new(Value::Identifier(*right_ident))),
    )
    .into();

    let Expression::Identifier(dest) = expression else {
        panic!();
    };

    // calculate length of output
    celf.set_size(*dest, Size::Static(left_length + right_length));

    *statement.borrow_mut() = Statement::Copy {
        expression: expression.clone(),
        value,
    }
}

fn eq_handler(
    _: &mut ResolveBitvectors,
    statement: Rc<RefCell<Statement>>,
    expression: &Expression,
    arguments: &[Rc<RefCell<Value>>],
) {
    // get identifiers and lengths of input bitvectors
    assert_eq!(arguments.len(), 2);

    let Value::Identifier(left_ident) = &*arguments[0].borrow() else {
        panic!();
    };

    let Value::Identifier(right_ident) = &*arguments[1].borrow() else {
        panic!();
    };

    // generate equality operation
    let value = Operation::Equal(
        Rc::new(RefCell::new(Value::Identifier(*left_ident))),
        Rc::new(RefCell::new(Value::Identifier(*right_ident))),
    )
    .into();

    let Expression::Identifier(_) = expression else {
        panic!();
    };

    *statement.borrow_mut() = Statement::Copy {
        expression: expression.clone(),
        value,
    }
}

fn undefined_handler(
    celf: &mut ResolveBitvectors,
    statement: Rc<RefCell<Statement>>,
    expression: &Expression,
    arguments: &[Rc<RefCell<Value>>],
) {
    // TODO: assign dest bitvector length to supplied argument
    // either by detecting const or evaluating what the value would be at that point
    // in execution (symbolic execution?)

    assert!(arguments.len() == 1);

    let Expression::Identifier(dest) = expression else {
        panic!();
    };

    let dest_size = celf.get_size(*dest).unwrap();

    if let Size::Unknown = dest_size {
        let Value::Identifier(size_ident) = &*arguments[0].borrow() else {
            panic!();
        };

        celf.set_size(*dest, Size::Runtime(*size_ident));
    }

    *statement.borrow_mut() = Statement::Copy {
        expression: expression.clone(),
        value: Literal::Int(0.into()).into(),
    }
}
