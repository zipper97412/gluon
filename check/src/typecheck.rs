//! The main typechecking interface which is responsible for typechecking expressions, patterns,
//! etc. Only checks which need to be aware of expressions are handled here the actual unifying and
//! checking of types are done in the `unify_type` and `kindcheck` modules.
use std::fmt;
use std::mem;

use base::scoped_map::ScopedMap;
use base::ast::{DisplayEnv, Expr, Literal, MutVisitor, Pattern, SpannedExpr};
use base::ast::{SpannedPattern, TypeBinding, TypedIdent, ValueBinding};
use base::error::Errors;
use base::fnv::{FnvMap, FnvSet};
use base::resolve;
use base::kind::{Kind, KindEnv, ArcKind};
use base::pos::{BytePos, Span, Spanned};
use base::symbol::{Symbol, SymbolRef, SymbolModule, Symbols};
use base::types::{self, Alias, AliasData, AppVec, ArcType, Field, Generic};
use base::types::{PrimitiveEnv, Type, TypeEnv, TypeVariable};

use kindcheck::{self, Error as KindCheckError, KindCheck, KindError};
use substitution::Substitution;
use rename::RenameError;
use unify::Error as UnifyError;
use unify;
use unify_type::{self, Error as UnifyTypeError, instantiate_generic_variables};

/// Type representing a single error when checking a type
#[derive(Debug, PartialEq)]
pub enum TypeError<I> {
    /// Variable has not been defined before it was used
    UndefinedVariable(I),
    /// Attempt to call a type which is not a function
    NotAFunction(ArcType<I>),
    /// Type has not been defined before it was used
    UndefinedType(I),
    /// Type were expected to have a certain field
    UndefinedField(ArcType<I>, I),
    /// Constructor type was found in a pattern but did not have the expected number of arguments
    PatternError(ArcType<I>, usize),
    /// Errors found when trying to unify two types
    Unification(ArcType<I>, ArcType<I>, Vec<UnifyTypeError<I>>),
    /// Error were found when trying to unify the kinds of two types
    KindError(KindCheckError<I>),
    /// Errors found during renaming (overload resolution)
    Rename(RenameError),
    /// Multiple types were declared with the same name in the same expression
    DuplicateTypeDefinition(I),
    /// A field was defined more than once in a record constructor or pattern match
    DuplicateField(I),
    /// Type is not a type which has any fields
    InvalidProjection(ArcType<I>),
    /// Expected to find a record with the following fields
    UndefinedRecord { fields: Vec<I> },
    /// Found a case expression without any alternatives
    EmptyCase,
    /// An `Error` expression was found indicating an invalid parse
    ErrorExpression,
}

impl<I> From<KindCheckError<I>> for TypeError<I>
    where I: PartialEq + Clone,
{
    fn from(e: KindCheckError<I>) -> TypeError<I> {
        match e {
            UnifyError::Other(KindError::UndefinedType(name)) => TypeError::UndefinedType(name),
            e => TypeError::KindError(e),
        }
    }
}

impl<I> From<RenameError> for TypeError<I> {
    fn from(e: RenameError) -> TypeError<I> {
        TypeError::Rename(e)
    }
}

impl<I: fmt::Display + AsRef<str>> fmt::Display for TypeError<I> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::TypeError::*;
        match *self {
            UndefinedVariable(ref name) => write!(f, "Undefined variable `{}`", name),
            NotAFunction(ref typ) => write!(f, "`{}` is not a function", typ),
            UndefinedType(ref name) => write!(f, "Type `{}` is not defined", name),
            UndefinedField(ref typ, ref field) => {
                write!(f, "Type `{}` does not have the field `{}`", typ, field)
            }
            Unification(ref expected, ref actual, ref errors) => {
                writeln!(f,
                         "Expected the following types to be equal\nExpected: {}\nFound: {}\n{} \
                          errors were found during unification:",
                         expected,
                         actual,
                         errors.len())?;
                if errors.is_empty() {
                    return Ok(());
                }
                for error in &errors[..errors.len() - 1] {
                    writeln!(f, "{}", error)?;
                }
                write!(f, "{}", errors.last().unwrap())
            }
            PatternError(ref typ, expected_len) => {
                write!(f, "Type {} has {} to few arguments", typ, expected_len)
            }
            KindError(ref err) => kindcheck::fmt_kind_error(err, f),
            Rename(ref err) => write!(f, "{}", err),
            DuplicateTypeDefinition(ref id) => {
                write!(f,
                       "Type '{}' has been already been defined in this module",
                       id)
            }
            DuplicateField(ref id) => {
                write!(f, "The record has more than one field named '{}'", id)
            }
            InvalidProjection(ref typ) => {
                write!(f,
                       "Type '{}' is not a type which allows field accesses",
                       typ)
            }
            UndefinedRecord { ref fields } => {
                write!(f, "No type found with the following fields: ")?;
                write!(f, "{}", fields[0])?;
                for field in &fields[1..] {
                    write!(f, ", {}", field)?;
                }
                Ok(())
            }
            EmptyCase => write!(f, "`case` expression with no alternatives"),
            ErrorExpression => write!(f, "`Error` expression found during typechecking"),
        }
    }
}

pub type SpannedTypeError<Id> = Spanned<TypeError<Id>, BytePos>;

type TcResult<T> = Result<T, TypeError<Symbol>>;

struct Environment<'a> {
    /// The global environment which the typechecker extracts types from
    environment: &'a (PrimitiveEnv + 'a),
    /// Stack allocated variables
    stack: ScopedMap<Symbol, ArcType>,
    /// Types which exist in some scope (`type Test = ... in ...`)
    stack_types: ScopedMap<Symbol, (ArcType, Alias<Symbol, ArcType>)>,
}

impl<'a> KindEnv for Environment<'a> {
    fn find_kind(&self, type_name: &SymbolRef) -> Option<ArcKind> {
        self.stack_types
            .get(type_name)
            .map(|&(_, ref alias)| {
                let mut kind = Kind::typ();
                for arg in alias.args.iter().rev() {
                    kind = Kind::function(arg.kind.clone(), kind);
                }
                kind
            })
            .or_else(|| self.environment.find_kind(type_name))
    }
}

impl<'a> TypeEnv for Environment<'a> {
    fn find_type(&self, id: &SymbolRef) -> Option<&ArcType> {
        self.stack.get(id).or_else(|| self.environment.find_type(id))
    }

    fn find_type_info(&self, id: &SymbolRef) -> Option<&Alias<Symbol, ArcType>> {
        self.stack_types
            .get(id)
            .map(|&(_, ref alias)| alias)
            .or_else(|| self.environment.find_type_info(id))
    }

    fn find_record(&self, fields: &[Symbol]) -> Option<(ArcType, ArcType)> {
        self.stack_types
            .iter()
            .find(|&(_, &(_, ref alias))| match **alias.unresolved_type() {
                Type::Record(ref row) => {
                    fields.iter()
                        .all(|name| row.row_iter().any(|f| f.name.name_eq(name)))
                }
                _ => false,
            })
            // FIXME Don't use unresolved_type
            .map(|t| ((t.1).0.clone(), (t.1).1.unresolved_type().clone()))
            .or_else(|| self.environment.find_record(fields))
    }
}

impl<'a> PrimitiveEnv for Environment<'a> {
    fn get_bool(&self) -> &ArcType {
        self.environment.get_bool()
    }
}

/// Type returned from the main typecheck function to make sure that nested `type` and `let`
/// expressions dont overflow the stack
enum TailCall {
    Type(ArcType),
    /// Returned from typechecking a `let` or `type` expresion to indicate that the expression body
    /// should be typechecked now.
    TailCall,
}

/// Struct which provides methods to typecheck expressions.
pub struct Typecheck<'a> {
    environment: Environment<'a>,
    symbols: SymbolModule<'a>,
    /// Mapping from the fresh symbol generated during typechecking to the symbol that was assigned
    /// during typechecking
    original_symbols: ScopedMap<Symbol, Symbol>,
    subs: Substitution<ArcType>,
    named_variables: FnvMap<Symbol, ArcType>,
    errors: Errors<SpannedTypeError<Symbol>>,
    /// Type variables `let test: a -> b` (`a` and `b`)
    type_variables: ScopedMap<Symbol, ArcType>,
}

/// Error returned when unsuccessfully typechecking an expression
pub type Error = Errors<SpannedTypeError<Symbol>>;

impl<'a> Typecheck<'a> {
    /// Create a new typechecker which typechecks expressions in `module`
    pub fn new(module: String,
               symbols: &'a mut Symbols,
               environment: &'a (PrimitiveEnv + 'a))
               -> Typecheck<'a> {
        let symbols = SymbolModule::new(module, symbols);
        Typecheck {
            environment: Environment {
                environment: environment,
                stack: ScopedMap::new(),
                stack_types: ScopedMap::new(),
            },
            symbols: symbols,
            original_symbols: ScopedMap::new(),
            subs: Substitution::new(),
            named_variables: FnvMap::default(),
            errors: Errors::new(),
            type_variables: ScopedMap::new(),
        }
    }

    fn error(&mut self, span: Span<BytePos>, error: TypeError<Symbol>) -> ArcType {
        self.errors.push(Spanned {
            span: span,
            value: error,
        });
        self.subs.new_var()
    }

    fn bool(&self) -> ArcType {
        self.environment.get_bool().clone()
    }

    fn find_at(&mut self, span: Span<BytePos>, id: &Symbol) -> ArcType {
        match self.find(id) {
            Ok(typ) => typ,
            Err(err) => self.error(span, err),
        }
    }

    fn find(&mut self, id: &Symbol) -> TcResult<ArcType> {
        match self.environment.find_type(id).map(ArcType::clone) {
            Some(typ) => {
                let typ = self.subs.set_type(typ);
                let typ = self.instantiate(&typ);
                debug!("Find {} : {}",
                       self.symbols.string(id),
                       types::display_type(&self.symbols, &typ));
                Ok(typ)
            }
            None => Err(TypeError::UndefinedVariable(id.clone())),
        }
    }

    fn find_record(&self, fields: &[Symbol]) -> TcResult<(ArcType, ArcType)> {
        self.environment
            .find_record(fields)
            .ok_or(TypeError::UndefinedRecord { fields: fields.to_owned() })
    }

    fn find_type_info(&self, id: &Symbol) -> TcResult<&Alias<Symbol, ArcType>> {
        self.environment.find_type_info(id).ok_or_else(|| TypeError::UndefinedType(id.clone()))
    }

    fn stack_var(&mut self, id: Symbol, typ: ArcType) {
        self.environment.stack.insert(id, typ);
    }

    fn stack_type(&mut self, id: Symbol, alias: &Alias<Symbol, ArcType>) {
        // Insert variant constructors into the local scope
        let aliased_type = alias.typ();
        if let Type::Variant(ref row) = **aliased_type {
            for field in row.row_iter().cloned() {
                let symbol = self.symbols.symbol(field.name.as_ref());
                self.original_symbols.insert(symbol, field.name.clone());
                self.stack_var(field.name, field.typ);
            }
        }
        let generic_args = alias.args
            .iter()
            .cloned()
            .map(Type::generic)
            .collect();
        let typ = Type::<_, ArcType>::app(alias.as_ref().clone(), generic_args);
        {
            // FIXME: Workaround so that both the types name in this module and its global
            // name are imported. Without this aliases may not be traversed properly
            self.environment.stack_types.insert(alias.name.clone(), (typ.clone(), alias.clone()));
        }
        self.environment.stack_types.insert(id, (typ, alias.clone()));
    }

    fn enter_scope(&mut self) {
        self.environment.stack.enter_scope();
        self.environment.stack_types.enter_scope();
        self.original_symbols.enter_scope();
    }

    fn exit_scope(&mut self) {
        self.environment.stack.exit_scope();
        self.environment.stack_types.exit_scope();
        self.original_symbols.exit_scope();
    }

    /// Generalizing updates all variables which are above `level` into "generic variables". A
    /// generic variable is instantiated with a fresh type variable each time it is refered to.
    ///
    /// Generalzing is a crucial part when inferring types as types will otherwise be less general
    /// than they need to. Consider the following expression.
    /// ```f#
    /// let id x = x
    /// in id 2
    /// ```
    /// If the variable in `id` was not generalized before the `id 2` call the variable would be
    /// unified to an `Int` which is not what we want. Generalazing before checking the body of the
    /// `let` basically infers that the variables in `id` does not refer to anything outside the
    /// `let` scope and can thus be "generalized" into `a -> a` which is instantiated with a fresh
    /// type variable in the `id 2` call.
    fn generalize_variables(&mut self,
                            level: u32,
                            args: &mut [TypedIdent<Symbol>],
                            expr: &mut SpannedExpr<Symbol>) {
        self.type_variables.enter_scope();

        // Replaces all type variables with their inferred types
        struct ReplaceVisitor<'a, 'b: 'a> {
            level: u32,
            tc: &'a mut Typecheck<'b>,
        }

        impl<'a, 'b> MutVisitor for ReplaceVisitor<'a, 'b> {
            type Ident = Symbol;

            fn visit_typ(&mut self, typ: &mut ArcType) {
                if let Some(finished) = self.tc.finish_type(self.level, typ) {
                    *typ = finished;
                }
            }
        }
        {
            let mut visitor = ReplaceVisitor {
                level: level,
                tc: self,
            };
            visitor.visit_expr(expr);
            for arg in args {
                visitor.visit_typ(&mut arg.typ)
            }
        }

        self.type_variables.exit_scope();
    }

    /// Typecheck `expr`. If successful the type of the expression will be returned and all
    /// identifiers in `expr` will be filled with the inferred type
    pub fn typecheck_expr(&mut self, expr: &mut SpannedExpr<Symbol>) -> Result<ArcType, Error> {
        self.typecheck_expr_expected(expr, None)
    }

    pub fn typecheck_expr_expected(&mut self,
                                   expr: &mut SpannedExpr<Symbol>,
                                   expected_type: Option<&ArcType>)
                                   -> Result<ArcType, Error> {
        fn tail_expr(e: &mut SpannedExpr<Symbol>) -> &mut SpannedExpr<Symbol> {
            match e.value {
                Expr::LetBindings(_, ref mut b) |
                Expr::TypeBindings(_, ref mut b) => tail_expr(b),
                _ => e,
            }
        }
        self.subs.clear();
        self.environment.stack.clear();

        let mut typ = self.typecheck(expr);
        if let Some(expected) = expected_type {
            let expected = self.create_unifiable_signature(expected.clone());
            typ = self.merge_signature(expr_check_span(expr), 0, &expected, typ);
        }
        typ = self.finish_type(0, &typ).unwrap_or(typ);
        typ = types::walk_move_type(typ, &mut unroll_typ);
        // Only the 'tail' expression need to be generalized at this point as all bindings
        // will have already been generalized
        self.generalize_variables(0, &mut [], tail_expr(expr));

        if self.errors.has_errors() {
            Err(mem::replace(&mut self.errors, Errors::new()))
        } else {
            match ::rename::rename(&mut self.symbols, &self.environment, expr) {
                Ok(()) => {
                    debug!("Typecheck result: {}", typ);
                    Ok(typ)
                }
                Err(errors) => {
                    for Spanned { span, value } in errors {
                        self.errors.push(Spanned {
                            span: span,
                            value: value.into(),
                        });
                    }
                    Err(mem::replace(&mut self.errors, Errors::new()))
                }
            }
        }
    }

    /// Main typechecking function. Returns the type of the expression if typechecking was
    /// successful
    fn typecheck(&mut self, mut expr: &mut SpannedExpr<Symbol>) -> ArcType {
        fn moving<T>(t: T) -> T {
            t
        }
        // How many scopes that have been entered in this "tailcall" loop
        let mut scope_count = 0;
        let returned_type;
        loop {
            match self.typecheck_(expr) {
                Ok(tailcall) => {
                    match tailcall {
                        TailCall::TailCall => {
                            // Call typecheck_ again with the next expression
                            expr = match moving(expr).value {
                                Expr::LetBindings(_, ref mut new_expr) |
                                Expr::TypeBindings(_, ref mut new_expr) => new_expr,
                                _ => panic!("Only Let and Type expressions can tailcall"),
                            };
                            scope_count += 1;
                        }
                        TailCall::Type(typ) => {
                            returned_type = typ;
                            break;
                        }
                    }
                }
                Err(err) => {
                    returned_type = self.subs.new_var();
                    self.errors.push(Spanned {
                        span: expr_check_span(expr),
                        value: err,
                    });
                    break;
                }
            }
        }
        for _ in 0..scope_count {
            self.exit_scope();
        }
        returned_type
    }

    fn typecheck_(&mut self,
                  expr: &mut SpannedExpr<Symbol>)
                  -> Result<TailCall, TypeError<Symbol>> {
        let expr_span = expr_check_span(expr);
        match expr.value {
            Expr::Ident(ref mut id) => {
                if let Some(new) = self.original_symbols.get(&id.name) {
                    id.name = new.clone();
                }
                id.typ = self.find(&id.name)?;
                Ok(TailCall::Type(id.typ.clone()))
            }
            Expr::Literal(ref lit) => {
                Ok(TailCall::Type(match *lit {
                    Literal::Int(_) => Type::int(),
                    Literal::Byte(_) => Type::byte(),
                    Literal::Float(_) => Type::float(),
                    Literal::String(_) => Type::string(),
                    Literal::Char(_) => Type::char(),
                }))
            }
            Expr::App(ref mut func, ref mut args) => {
                let mut func_type = self.typecheck(&mut **func);
                for arg in args.iter_mut() {
                    let f = Type::function(vec![self.subs.new_var()], self.subs.new_var());
                    func_type = self.unify(&f, func_type)?;
                    func_type = match func_type.as_function() {
                        Some((arg_ty, ret_ty)) => {
                            let actual = self.typecheck(arg);
                            self.unify_span(expr_check_span(arg), arg_ty, actual);
                            ret_ty.clone()
                        }
                        None => return Err(TypeError::NotAFunction(func_type.clone())),
                    };
                }
                Ok(TailCall::Type(func_type))
            }
            Expr::IfElse(ref mut pred, ref mut if_true, ref mut if_false) => {
                let pred_type = self.typecheck(&mut **pred);
                let bool_type = self.bool();
                self.unify_span(expr_check_span(pred), &bool_type, pred_type);

                // Both branches must unify to the same type
                let true_type = self.typecheck(&mut **if_true);
                let false_type = self.typecheck(&mut **if_false);
                self.unify(&true_type, false_type).map(TailCall::Type)
            }
            Expr::Infix(ref mut lhs, ref mut op, ref mut rhs) => {
                let lhs_type = self.typecheck(&mut **lhs);
                let rhs_type = self.typecheck(&mut **rhs);
                let op_name = String::from(self.symbols.string(&op.value.name));
                let return_type = if op_name.starts_with('#') {
                    // Handle primitives
                    let arg_type = self.unify(&lhs_type, rhs_type)?;
                    let op_type = op_name.trim_matches(|c: char| !c.is_alphabetic());
                    let prim_type = primitive_type(op_type);
                    let typ = self.unify(&prim_type, arg_type)?;
                    let return_type = match &op_name[1 + op_type.len()..] {
                        "+" | "-" | "*" | "/" => typ,
                        "==" | "<" => self.bool(),
                        _ => return Err(TypeError::UndefinedVariable(op.value.name.clone())),
                    };
                    op.value.typ = Type::function(vec![prim_type.clone(), prim_type.clone()],
                                                  return_type.clone());
                    return_type
                } else {
                    match &*op_name {
                        "&&" | "||" => {
                            self.unify(&lhs_type, rhs_type.clone())?;
                            op.value.typ = Type::function(vec![self.bool(), self.bool()],
                                                          self.bool());
                            self.unify(&self.bool(), lhs_type)?
                        }
                        _ => {
                            op.value.typ = self.find(&op.value.name)?;
                            let func_type = Type::function(vec![lhs_type, rhs_type],
                                                           self.subs.new_var());
                            let ret = self.unify(&op.value.typ, func_type)?
                                .as_function()
                                .and_then(|(_, ret)| ret.as_function())
                                .map(|(_, ret)| ret.clone())
                                .expect("ICE: unify binop");

                            ret
                        }
                    }
                };
                Ok(TailCall::Type(return_type))
            }
            Expr::Tuple(ref mut exprs) => {
                assert!(exprs.len() == 0);
                Ok(TailCall::Type(Type::unit()))
            }
            Expr::Match(ref mut expr, ref mut alts) => {
                let typ = self.typecheck(&mut **expr);
                let mut expected_alt_type = None;

                for alt in alts.iter_mut() {
                    self.enter_scope();
                    self.typecheck_pattern(&mut alt.pattern, typ.clone());
                    let mut alt_type = self.typecheck(&mut alt.expr);
                    self.exit_scope();
                    // All alternatives must unify to the same type
                    if let Some(ref expected) = expected_alt_type {
                        alt_type = self.unify(expected, alt_type)?;
                    }
                    expected_alt_type = Some(alt_type);
                }
                expected_alt_type.ok_or(TypeError::EmptyCase).map(TailCall::Type)
            }
            Expr::LetBindings(ref mut bindings, _) => {
                self.typecheck_bindings(bindings)?;
                Ok(TailCall::TailCall)
            }
            Expr::Projection(ref mut expr, ref field_id, ref mut ast_field_typ) => {
                let mut expr_typ = self.typecheck(&mut **expr);
                debug!("Projection {} . {:?}",
                       types::display_type(&self.symbols, &expr_typ),
                       self.symbols.string(field_id));
                self.subs.make_real(&mut expr_typ);
                if let Type::Variable(_) = *expr_typ {
                    // Eagerly attempt to find a record with `field_access` since infering to just
                    // a polymorphic record may cause some code to fail to infer such as
                    // the test `row_polymorphism::late_merge_with_signature`
                    if let Ok(record_type) = self.find_record(&[field_id.clone()])
                        .map(|t| t.0.clone()) {
                        let record_type = self.instantiate(&record_type);
                        expr_typ = self.unify(&record_type, expr_typ)?;
                    }
                }
                let record = self.remove_aliases(expr_typ.clone());
                match *record {
                    Type::Variable(_) |
                    Type::Record(_) => {
                        let field_type = record.row_iter()
                            .find(|field| field.name.name_eq(field_id))
                            .map(|field| field.typ.clone());
                        *ast_field_typ = match field_type {
                            Some(typ) => self.instantiate(&typ),
                            None => {
                                // FIXME As the polymorphic `record_type` do not have the type
                                // fields which `typ` this unification is only done after we
                                // checked if the field exists which lets field accesses on
                                // types with type fields still work
                                let field_var = self.subs.new_var();
                                let field = Field::new(field_id.clone(), field_var.clone());
                                let record_type =
                                    Type::poly_record(vec![], vec![field], self.subs.new_var());
                                self.unify(&record_type, record)?;
                                field_var
                            }
                        };
                        Ok(TailCall::Type(ast_field_typ.clone()))
                    }
                    _ => Err(TypeError::InvalidProjection(record)),
                }
            }
            Expr::Array(ref mut array) => {
                let mut expected_type = self.subs.new_var();
                for expr in &mut array.exprs {
                    let typ = self.typecheck(expr);
                    expected_type = self.unify_span(expr.span, &expected_type, typ);
                }
                array.typ = Type::array(expected_type);
                Ok(TailCall::Type(array.typ.clone()))
            }
            Expr::Lambda(ref mut lambda) => {
                let loc = format!("lambda:{}", expr.span.start);
                lambda.id.name = self.symbols.symbol(loc);
                let function_type = self.subs.new_var();
                let typ = self.typecheck_lambda(function_type, &mut lambda.args, &mut lambda.body);
                lambda.id.typ = typ.clone();
                Ok(TailCall::Type(typ))
            }
            Expr::TypeBindings(ref mut bindings, ref expr) => {
                self.typecheck_type_bindings(bindings, expr)?;
                Ok(TailCall::TailCall)
            }
            Expr::Record { ref mut typ, ref mut types, exprs: ref mut fields } => {
                let mut new_types: Vec<Field<_, _>> = Vec::with_capacity(types.len());

                let mut duplicated_fields = FnvSet::default();
                for field in types {
                    if let Some(ref mut typ) = field.value {
                        *typ = self.create_unifiable_signature(typ.clone());
                    }
                    let alias = self.find_type_info(&field.name)?.clone();
                    if self.error_on_duplicated_field(&mut duplicated_fields,
                                                      expr_span,
                                                      field.name.clone()) {
                        new_types.push(Field::new(field.name.clone(), alias));
                    }
                }

                let mut new_fields: Vec<Field<_, _>> = Vec::with_capacity(fields.len());
                for field in fields {
                    let typ = match field.value {
                        Some(ref mut expr) => self.typecheck(expr),
                        None => self.find(&field.name)?,
                    };
                    if self.error_on_duplicated_field(&mut duplicated_fields,
                                                      expr_span,
                                                      field.name.clone()) {
                        new_fields.push(Field::new(field.name.clone(), typ));
                    }
                }
                let result = self.find_record(&new_fields.iter()
                        .map(|f| f.name.clone())
                        .collect::<Vec<_>>())
                    .map(|t| (t.0.clone(), t.1.clone()));
                let (id_type, record_type) = match result {
                    Ok(x) => x,
                    Err(_) => {
                        *typ = Type::record(new_types, new_fields);
                        return Ok(TailCall::Type(typ.clone()));
                    }
                };
                let id_type = self.instantiate(&id_type);
                let record_type = instantiate_generic_variables(&mut self.named_variables,
                                                                &self.subs,
                                                                &record_type);
                self.unify(&Type::record(new_types, new_fields), record_type)?;
                *typ = id_type.clone();
                Ok(TailCall::Type(id_type.clone()))
            }
            Expr::Block(ref mut exprs) => {
                let (last, exprs) = exprs.split_last_mut().expect("Expr in block");
                for expr in exprs {
                    self.typecheck(expr);
                }
                Ok(TailCall::Type(self.typecheck(last)))
            }
            Expr::Error => Err(TypeError::ErrorExpression),
        }
    }

    fn typecheck_lambda(&mut self,
                        function_type: ArcType,
                        args: &mut [TypedIdent],
                        body: &mut SpannedExpr<Symbol>)
                        -> ArcType {
        self.enter_scope();
        let mut arg_types = Vec::new();
        {
            let mut iter1 = function_arg_iter(self, function_type);
            let mut iter2 = args.iter_mut();
            while let (Some(arg_type), Some(arg)) = (iter1.next(), iter2.next()) {
                arg.typ = arg_type;
                arg_types.push(arg.typ.clone());
                iter1.tc.stack_var(arg.name.clone(), arg.typ.clone());
            }
        }
        let body_type = self.typecheck(body);
        self.exit_scope();
        Type::function(arg_types, body_type)
    }

    fn typecheck_pattern(&mut self,
                         pattern: &mut SpannedPattern<Symbol>,
                         match_type: ArcType)
                         -> ArcType {
        let span = pattern.span;
        match pattern.value {
            Pattern::Constructor(ref mut id, ref mut args) => {
                if let Some(new) = self.original_symbols.get(&id.name) {
                    id.name = new.clone();
                }
                // Find the enum constructor and return the types for its arguments
                let ctor_type = self.find_at(span, &id.name);
                id.typ = ctor_type.clone();
                let return_type = match self.typecheck_pattern_rec(args, ctor_type) {
                    Ok(return_type) => return_type,
                    Err(err) => self.error(span, err),
                };
                self.unify_span(span, &match_type, return_type)
            }
            Pattern::Record { typ: ref mut curr_typ,
                              types: ref mut associated_types,
                              ref mut fields } => {
                *curr_typ = match_type.clone();
                let mut match_type = self.remove_alias(match_type);

                let mut pattern_fields = Vec::with_capacity(associated_types.len() + fields.len());

                let mut duplicated_fields = FnvSet::default();
                {
                    let all_fields = associated_types.iter()
                        .map(|field| &field.0)
                        .chain(fields.iter().map(|field| &field.0));
                    for field in all_fields {
                        if self.error_on_duplicated_field(&mut duplicated_fields,
                                                          span,
                                                          field.clone()) {
                            pattern_fields.push(field.clone());
                        }
                    }
                }

                // actual_type is the record (not hidden behind an alias)
                let (mut typ, mut actual_type) = match self.find_record(&pattern_fields)
                    .map(|t| (t.0.clone(), t.1.clone())) {
                    Ok(typ) => typ,
                    Err(_) => {
                        // HACK Since there is no way to unify just the name of the 'type field's we
                        // need to take the types from the matched type. This leaves the `types`
                        // list incomplete however since it may miss some fields defined in the
                        // pattern. These are catched later in this function.
                        let types = match_type.type_field_iter()
                            .filter(|field| {
                                associated_types.iter().any(|other| other.0.name_eq(&field.name))
                            })
                            .cloned()
                            .collect();

                        let fields = fields.iter()
                            .map(|&(ref id, _)| Field::new(id.clone(), self.subs.new_var()))
                            .collect();
                        let t = Type::poly_record(types, fields, self.subs.new_var());
                        (t.clone(), t)
                    }
                };
                typ = self.instantiate(&typ);
                actual_type = instantiate_generic_variables(&mut self.named_variables,
                                                            &self.subs,
                                                            &actual_type);
                self.unify_span(span, &match_type, typ);
                match_type = actual_type;

                for field in fields {
                    let name = &field.0;
                    // The field should always exist since the type was constructed from the pattern
                    let field_type = match_type.row_iter()
                        .find(|f| f.name.name_eq(name))
                        .expect("ICE: Expected field to exist in type")
                        .typ
                        .clone();
                    match field.1 {
                        Some(ref mut pattern) => {
                            self.typecheck_pattern(pattern, field_type);
                        }
                        None => {
                            self.stack_var(name.clone(), field_type);
                        }
                    }
                }

                // Check that all types declared in the pattern exists
                for field in associated_types.iter_mut() {
                    let name = field.1
                        .as_ref()
                        .unwrap_or(&field.0)
                        .clone();
                    // The `types` in the record type should have a type matching the
                    // `name`
                    let field_type = match_type.type_field_iter()
                        .find(|field| field.name.name_eq(&name));
                    match field_type {
                        Some(field_type) => {
                            // This forces refresh_type to remap the name a type was given
                            // in this module to its actual name
                            self.original_symbols.insert(name.clone(), field_type.typ.name.clone());
                            self.stack_type(name, &field_type.typ);
                        }
                        None => {
                            self.error(span, TypeError::UndefinedField(match_type.clone(), name));
                        }
                    }
                }

                match_type
            }
            Pattern::Ident(ref mut id) => {
                self.stack_var(id.name.clone(), match_type.clone());
                id.typ = match_type.clone();
                match_type
            }
        }
    }

    fn typecheck_pattern_rec(&mut self,
                             args: &mut [SpannedPattern<Symbol>],
                             typ: ArcType)
                             -> TcResult<ArcType> {
        let len = args.len();
        match args.split_first_mut() {
            Some((head, tail)) => {
                match typ.as_function() {
                    Some((arg, ret)) => {
                        self.typecheck_pattern(head, arg.clone());
                        self.typecheck_pattern_rec(tail, ret.clone())
                    }
                    None => Err(TypeError::PatternError(typ.clone(), len)),
                }
            }
            None => Ok(typ),
        }
    }

    fn typecheck_bindings(&mut self, bindings: &mut [ValueBinding<Symbol>]) -> TcResult<()> {
        self.enter_scope();
        self.type_variables.enter_scope();
        let level = self.subs.var_id();
        let is_recursive = bindings.iter().all(|bind| !bind.args.is_empty());
        // When the definitions are allowed to be mutually recursive
        if is_recursive {
            for bind in bindings.iter_mut() {
                let typ = {
                    bind.typ = self.create_unifiable_signature(bind.typ.clone());
                    self.kindcheck(&mut bind.typ)?;
                    self.instantiate_signature(&bind.typ)
                };
                self.typecheck_pattern(&mut bind.name, typ);
                if let Expr::Lambda(ref mut lambda) = bind.expr.value {
                    if let Pattern::Ident(ref name) = bind.name.value {
                        lambda.id.name = name.name.clone();
                    }
                }
            }
        }
        let mut types = Vec::new();
        for bind in bindings.iter_mut() {
            self.type_variables.enter_scope();

            // Functions which are declared as `let f x = ...` are allowed to be self
            // recursive
            let mut typ = if bind.args.is_empty() {
                self.instantiate_signature(&bind.typ);
                bind.typ = self.create_unifiable_signature(bind.typ.clone());
                self.kindcheck(&mut bind.typ)?;
                self.typecheck(&mut bind.expr)
            } else {
                let function_typ = self.instantiate(&bind.typ);
                self.typecheck_lambda(function_typ, &mut bind.args, &mut bind.expr)
            };

            debug!("let {:?} : {}",
                   bind.name,
                   types::display_type(&self.symbols, &typ));

            typ = self.merge_signature(bind.name.span, level, &bind.typ, typ);

            if !is_recursive {
                // Merge the type declaration and the actual type
                self.generalize_variables(level, &mut bind.args, &mut bind.expr);
                self.typecheck_pattern(&mut bind.name, typ);
            } else {
                types.push(typ);
            }

            self.type_variables.exit_scope();
        }
        if is_recursive {
            for (found_typ, bind) in types.into_iter().zip(bindings.iter_mut()) {
                // Merge the variable we bound to the name and the type inferred
                // in the expression
                self.unify_span(bind.name.span, &bind.typ, found_typ);
            }
        }
        // Once all variables inside the let has been unified we can quantify them
        debug!("Generalize {}", level);
        for bind in bindings {
            self.generalize_variables(level, &mut bind.args, &mut bind.expr);
            if let Some(typ) = self.finish_type(level, &bind.typ) {
                bind.typ = typ;
            }
            self.finish_pattern(level, &mut bind.name, &bind.typ);
        }
        debug!("Typecheck `in`");
        self.type_variables.exit_scope();
        Ok(())
    }

    fn typecheck_type_bindings(&mut self,
                               bindings: &mut [TypeBinding<Symbol>],
                               expr: &SpannedExpr<Symbol>)
                               -> TcResult<()> {
        self.enter_scope();

        // Rename the types so they get a name which is distinct from types from other
        // modules
        for bind in bindings.iter_mut() {
            let s = String::from(self.symbols.string(&bind.alias.name));
            let new = self.symbols.scoped_symbol(&s);
            self.original_symbols.insert(bind.alias.name.clone(), new.clone());
            // Rename the aliase's name to its global name
            bind.alias.name = new;
        }

        for bind in bindings.iter_mut() {
            *bind.alias.unresolved_type_mut() =
                self.create_unifiable_signature(bind.alias.unresolved_type().clone());
        }

        {
            let mut check = KindCheck::new(&self.environment, &self.symbols);

            // Setup kind variables for all holes and insert the types in the
            // the type expression into the kindcheck environment
            for bind in bindings.iter_mut() {
                // Create the kind for this binding
                // Test a b : 2 -> 1 -> Type
                // and bind the same variables to the arguments of the type binding
                // ('a' and 'b' in the example)
                let mut id_kind = check.type_kind();
                for generic in bind.alias
                    .args
                    .iter_mut()
                    .rev() {
                    check.instantiate_kinds(&mut generic.kind);
                    id_kind = Kind::function(generic.kind.clone(), id_kind);
                }
                check.add_local(bind.alias.name.clone(), id_kind);
            }

            // Kindcheck all the types in the environment
            for bind in bindings.iter_mut() {
                check.set_variables(&bind.alias.args);
                check.kindcheck_type(bind.alias.unresolved_type_mut())?;
            }

            // All kinds are now inferred so replace the kinds store in the AST
            for bind in bindings.iter_mut() {
                let alias = &mut bind.alias;
                *alias.unresolved_type_mut() = check.finalize_type(alias.unresolved_type().clone());
                for arg in &mut alias.args {
                    *arg = check.finalize_generic(arg);
                }
            }
            let alias_group =
                Alias::group(bindings.iter().map(|bind| bind.alias.clone()).collect());
            for (bind, alias) in bindings.iter_mut().zip(alias_group) {
                bind.finalized_alias = Some(alias);
            }
        }

        // Finally insert the declared types into the global scope
        for bind in bindings {
            if self.environment
                .stack_types
                .get(&bind.name)
                .is_some() {
                self.errors.push(Spanned {
                    span: expr_check_span(expr),
                    value: TypeError::DuplicateTypeDefinition(bind.name.clone()),
                });
            } else {
                self.stack_type(bind.name.clone(), &bind.finalized_alias.as_ref().unwrap());
            }
        }

        Ok(())
    }

    fn kindcheck(&self, typ: &mut ArcType) -> TcResult<()> {
        let mut check = KindCheck::new(&self.environment, &self.symbols);
        check.kindcheck_type(typ)?;
        Ok(())
    }

    fn finish_pattern(&mut self, level: u32, pattern: &mut SpannedPattern<Symbol>, typ: &ArcType) {
        match pattern.value {
            Pattern::Ident(ref mut id) => {
                if let Some(typ) = self.finish_type(level, &id.typ) {
                    id.typ = typ;
                }
                debug!("{}: {}",
                       self.symbols.string(&id.name),
                       types::display_type(&self.symbols, &id.typ));
                self.intersect_type(level, &id.name, &id.typ);
            }
            Pattern::Record { ref mut typ, ref mut fields, .. } => {
                debug!("{{ .. }}: {}", types::display_type(&self.symbols, typ));
                if let Some(finished) = self.finish_type(level, typ) {
                    *typ = finished;
                }
                let record_type = self.remove_alias(typ.clone());
                with_pattern_types(fields,
                                   &record_type,
                                   |field_name, binding, field_type| match *binding {
                                       Some(ref mut pat) => {
                                           self.finish_pattern(level, pat, field_type);
                                       }
                                       None => {
                                           self.intersect_type(level, field_name, field_type);
                                       }
                                   });
            }
            Pattern::Constructor(ref id, ref mut args) => {
                debug!("{}: {}",
                       self.symbols.string(&id.name),
                       types::display_type(&self.symbols, typ));
                for (arg, arg_type) in args.iter_mut()
                    .zip(function_arg_iter(self, typ.clone()).collect::<Vec<_>>()) {
                    self.finish_pattern(level, arg, &arg_type);
                }
            }
        }
    }

    fn intersect_type(&mut self, level: u32, symbol: &Symbol, symbol_type: &ArcType) {
        let typ = {
            let existing_types = self.environment
                .stack
                .get_all(symbol)
                .expect("Symbol is not in scope");
            if existing_types.len() >= 2 {
                let existing_type = &existing_types[existing_types.len() - 2];
                debug!("Intersect `{}`\n{} ∩ {}",
                       symbol,
                       types::display_type(&self.symbols, self.subs.real(existing_type)),
                       types::display_type(&self.symbols, self.subs.real(symbol_type)));
                let state = unify_type::State::new(&self.environment, &self.subs);
                let result = unify::intersection(&self.subs, state, existing_type, symbol_type);
                debug!("Intersect result {}", result);
                result
            } else {
                symbol_type.clone()
            }
        };
        *self.environment
            .stack
            .get_mut(symbol)
            .unwrap() = self.finish_type(level, &typ).unwrap_or(typ)
    }

    /// Generate a generic variable name which is not used in the current scope
    fn next_variable(&mut self, level: u32, s: &mut String) {
        for c in b'a'..(b'z' + 1) {
            s.push(c as char);
            let symbol = self.symbols.symbol(&s[..]);
            if self.type_variables.get(&symbol).is_none() {
                self.type_variables.insert(symbol,
                                           Type::variable(TypeVariable {
                                               id: level,
                                               kind: Kind::typ(),
                                           }));
                return;
            }
            s.pop();
        }
        s.push('a');
        self.next_variable(level, s)
    }

    /// Finish a type by replacing all unbound type variables above `level` with generics
    fn finish_type(&mut self, level: u32, typ: &ArcType) -> Option<ArcType> {
        let mut generic = None;
        let mut i = 0;
        self.finish_type_(level, &mut generic, &mut i, typ)
    }

    fn finish_type_(&mut self,
                    level: u32,
                    generic: &mut Option<String>,
                    i: &mut i32,
                    typ: &Type<Symbol>)
                    -> Option<ArcType> {
        use base::types::TypeVisitor;

        let mut visitor = types::ControlVisitation(|typ: &Type<_, _>| {
            let replacement = self.subs
                .replace_variable(typ)
                .map(|t| self.finish_type_(level, generic, i, &t).unwrap_or(t));
            let mut typ = typ;
            if let Some(ref t) = replacement {
                debug!("{} ==> {}",
                       types::display_type(&self.symbols, &typ),
                       types::display_type(&self.symbols, t));
                typ = &**t;
            }
            match *typ {
                Type::Variable(ref var) if self.subs.get_level(var.id) >= level => {
                    // Create a prefix if none exists
                    if generic.is_none() {
                        let mut g = String::new();
                        self.next_variable(level, &mut g);
                        *generic = Some(g);
                    }
                    let generic = generic.as_ref().unwrap();

                    let generic = format!("{}{}", generic, i);
                    *i += 1;
                    let id = self.symbols.symbol(generic);
                    let gen: ArcType = Type::generic(Generic::new(id.clone(), var.kind.clone()));
                    self.subs.insert(var.id, gen.clone());
                    Some(gen)
                }
                Type::ExtendRow { ref types, ref fields, ref rest } => {
                    let new_fields = types::walk_move_types(fields, |field| {
                        // Make a new name base for any unbound variables in the record field
                        // Gives { id : a0 -> a0, const : b0 -> b1 -> b1 }
                        // instead of { id : a0 -> a0, const : a1 -> a2 -> a2 }
                        self.finish_type(level, &field.typ)
                            .map(|typ| Field::new(field.name.clone(), typ))
                    });
                    let new_rest = self.finish_type(level, rest);
                    types::merge(fields,
                                 new_fields,
                                 rest,
                                 new_rest,
                                 |fields, rest| Type::extend_row(types.clone(), fields, rest))
                        .or_else(|| replacement.clone())
                }
                _ => {
                    let new_type =
                        types::walk_move_type_opt(typ,
                                                  &mut |typ: &Type<Symbol>| {
                                                      self.finish_type_(level, generic, i, typ)
                                                  });
                    new_type.map(|t| unroll_typ(&t).unwrap_or(t)).or_else(|| replacement.clone())
                }
            }
        });
        visitor.visit(typ)
    }

    fn instantiate_signature(&mut self, typ: &ArcType) -> ArcType {
        let typ = self.instantiate(typ);
        // Put all new generic variable names into scope
        for (generic, variable) in &self.named_variables {
            if self.type_variables.get(generic).is_none() {
                self.type_variables.insert(generic.clone(), variable.clone());
            }
        }
        typ
    }

    // Replaces `Type::Id` types with the actual `Type::Alias` type it refers to
    // Replaces variant names with the actual symbol they should refer to
    // Instantiates Type::Hole with a fresh type variable to ensure the hole only ever refers to a
    // single type variable
    fn create_unifiable_signature(&mut self, typ: ArcType) -> ArcType {
        let mut f = |typ: &Type<Symbol, ArcType>| {
            match *typ {
                Type::Ident(ref id) => {
                    // Substitute the Id by its alias if possible
                    let new_id = self.original_symbols.get(id).unwrap_or(id);
                    self.environment
                        .find_type_info(new_id)
                        .map(|alias| alias.clone().into_type())
                        .or_else(|| if id == new_id {
                            None
                        } else {
                            Some(Type::ident(new_id.clone()))
                        })
                }
                Type::Variant(ref row) => {
                    let iter = || row.row_iter().map(|var| self.original_symbols.get(&var.name));
                    if iter().any(|opt| opt.is_some()) {
                        // If any of the variants requires a symbol replacement
                        // we create a new type
                        Some(Type::variant(iter()
                            .zip(row.row_iter())
                            .map(|(new, old)| match new {
                                Some(new) => Field::new(new.clone(), old.typ.clone()),
                                None => old.clone(),
                            })
                            .collect()))
                    } else {
                        None
                    }
                }
                Type::Hole => Some(self.subs.new_var()),
                _ => None,
            }
        };
        types::walk_move_type(typ, &mut f)
    }

    fn merge_signature(&mut self,
                       span: Span<BytePos>,
                       level: u32,
                       expected: &ArcType,
                       mut actual: ArcType)
                       -> ArcType {
        let state = unify_type::State::new(&self.environment, &self.subs);
        match unify_type::merge_signature(&self.subs,
                                          &mut self.type_variables,
                                          level,
                                          state,
                                          expected,
                                          &actual) {
            Ok(typ) => self.subs.set_type(typ),
            Err(errors) => {
                let mut expected = expected.clone();
                expected = self.subs.set_type(expected);
                actual = self.subs.set_type(actual);
                let err = TypeError::Unification(expected, actual, apply_subs(&self.subs, errors));
                self.errors.push(Spanned {
                    span: span,
                    value: err,
                });
                self.subs.new_var()
            }
        }
    }

    fn unify_span(&mut self, span: Span<BytePos>, expected: &ArcType, actual: ArcType) -> ArcType {
        match self.unify(expected, actual) {
            Ok(typ) => typ,
            Err(err) => {
                self.errors.push(Spanned {
                    span: span,
                    value: err,
                });
                self.subs.new_var()
            }
        }
    }

    fn unify(&self, expected: &ArcType, mut actual: ArcType) -> TcResult<ArcType> {
        debug!("Unify {} <=> {}",
               types::display_type(&self.symbols, expected),
               types::display_type(&self.symbols, &actual));
        let state = unify_type::State::new(&self.environment, &self.subs);
        match unify::unify(&self.subs, state, expected, &actual) {
            Ok(typ) => Ok(self.subs.set_type(typ)),
            Err(errors) => {
                let mut expected = expected.clone();
                expected = self.subs.set_type(expected);
                actual = self.subs.set_type(actual);
                debug!("Error '{:?}' between:\n>> {}\n>> {}",
                       errors,
                       types::display_type(&self.symbols, &expected),
                       types::display_type(&self.symbols, &actual));
                Err(TypeError::Unification(expected, actual, apply_subs(&self.subs, errors)))
            }
        }
    }

    fn remove_alias(&self, typ: ArcType) -> ArcType {
        resolve::remove_alias(&self.environment, &typ).unwrap_or(None).unwrap_or(typ)
    }

    fn remove_aliases(&self, typ: ArcType) -> ArcType {
        resolve::remove_aliases(&self.environment, typ)
    }

    fn instantiate(&mut self, typ: &ArcType) -> ArcType {
        self.named_variables.clear();
        instantiate_generic_variables(&mut self.named_variables, &self.subs, typ)
    }

    fn error_on_duplicated_field(&mut self,
                                 duplicated_fields: &mut FnvSet<Symbol>,
                                 span: Span<BytePos>,
                                 name: Symbol)
                                 -> bool {
        duplicated_fields.replace(name).map_or(true, |name| {
            self.errors.push(Spanned {
                span: span,
                value: TypeError::DuplicateField(name),
            });
            false
        })
    }
}

fn with_pattern_types<F>(fields: &mut [(Symbol, Option<SpannedPattern<Symbol>>)],
                         typ: &ArcType,
                         mut f: F)
    where F: FnMut(&Symbol, &mut Option<SpannedPattern<Symbol>>, &ArcType),
{
    for field in fields {
        // If the field in the pattern does not exist (undefined field error) then skip it as
        // the error itself will already have been reported
        let opt = typ.row_iter().find(|type_field| type_field.name.name_eq(&field.0));
        if let Some(associated_type) = opt {
            f(&field.0, &mut field.1, &associated_type.typ);
        }
    }
}

fn apply_subs(subs: &Substitution<ArcType>,
              errors: Errors<UnifyTypeError<Symbol>>)
              -> Vec<UnifyTypeError<Symbol>> {
    use unify::Error::*;
    errors.into_iter()
        .map(|error| match error {
            TypeMismatch(expected, actual) => {
                TypeMismatch(subs.set_type(expected), subs.set_type(actual))
            }
            Occurs(var, typ) => Occurs(var, subs.set_type(typ)),
            Other(err) => Other(err),
        })
        .collect()
}

pub fn extract_generics(args: &[ArcType]) -> Vec<Generic<Symbol>> {
    args.iter()
        .map(|arg| match **arg {
            Type::Generic(ref gen) => gen.clone(),
            _ => panic!("The type on the lhs of a type binding did not have all generic arguments"),
        })
        .collect()
}

fn get_alias_app<'a>(env: &'a TypeEnv,
                     typ: &'a ArcType)
                     -> Option<(&'a AliasData<Symbol, ArcType>, &'a [ArcType])> {
    match **typ {
        Type::Alias(ref alias) => Some((alias, &[][..])),
        Type::App(ref alias, ref args) => {
            match **alias {
                Type::Alias(ref alias) => Some((alias, args)),
                _ => None,
            }
        }
        _ => {
            typ.alias_ident()
                .and_then(|id| env.find_type_info(id).map(|alias| (&**alias, typ.unapplied_args())))
        }
    }
}

struct FunctionArgIter<'a, 'b: 'a> {
    tc: &'a mut Typecheck<'b>,
    typ: ArcType,
}

impl<'a, 'b> Iterator for FunctionArgIter<'a, 'b> {
    type Item = ArcType;
    fn next(&mut self) -> Option<ArcType> {
        loop {
            let (arg, new) = match self.typ.as_function() {
                Some((arg, ret)) => (Some(arg.clone()), ret.clone()),
                None => {
                    match get_alias_app(&self.tc.environment, &self.typ) {
                        Some((alias, args)) => {
                            match resolve::type_of_alias(&self.tc.environment, alias, args) {
                                Some(typ) => (None, typ.clone()),
                                None => return None,
                            }
                        }
                        None => return Some(self.tc.subs.new_var()),
                    }
                }
            };
            self.typ = new;
            if let Some(arg) = arg {
                return Some(arg);
            }
        }
    }
}

fn function_arg_iter<'a, 'b>(tc: &'a mut Typecheck<'b>, typ: ArcType) -> FunctionArgIter<'a, 'b> {
    FunctionArgIter { tc: tc, typ: typ }
}

fn primitive_type(op_type: &str) -> ArcType {
    match op_type {
        "Int" => Type::int(),
        "Float" => Type::float(),
        "Char" => Type::char(),
        "Byte" => Type::byte(),
        _ => panic!("ICE: Unknown primitive type"),
    }
}

/// Returns a span of the innermost expression of a group of nested `let` and `type` bindings.
/// This span is useful for more precisely marking the span of a type error.
///
/// ```ignore
/// let x: Int =
///     let y = 1.0
///     ~~~~~~~~~~~
///     y
///     ~
///     ^
/// x
/// ```
fn expr_check_span(e: &SpannedExpr<Symbol>) -> Span<BytePos> {
    match e.value {
        Expr::LetBindings(_, ref b) |
        Expr::TypeBindings(_, ref b) => expr_check_span(b),
        _ => e.span,
    }
}

/// Removes layers of `Type::App` and `Type::Record` by packing them into a single `Type::App` or
/// `Type::Record`
///
/// Example:
///
/// ```rust
/// #[macro_use]
/// extern crate collect_mac;
/// extern crate gluon_base;
/// extern crate gluon_check;
///
/// use gluon_base::types::{Type, ArcType, BuiltinType};
/// use gluon_check::typecheck::unroll_typ;
///
/// # fn main() {
/// let i: ArcType = Type::int();
/// let s: ArcType = Type::string();
/// assert_eq!(unroll_typ(&*Type::app(Type::app(i.clone(), collect![s.clone()]), collect![i.clone()])),
///            Some(Type::app(i.clone(), collect![s.clone(), i.clone()])));
/// assert_eq!(unroll_typ(&*Type::app(Type::app(i.clone(), collect![i.clone()]), collect![s.clone()])),
///            Some(Type::app(i.clone(), collect![i.clone(), s.clone()])));
/// let f: ArcType = Type::builtin(BuiltinType::Function);
/// assert_eq!(unroll_typ(&*Type::app(Type::app(f.clone(), collect![i.clone()]), collect![s.clone()])),
///            Some(Type::function(collect![i.clone()], s.clone())));
/// # }
/// ```
pub fn unroll_typ(typ: &Type<Symbol>) -> Option<ArcType> {
    let mut args = AppVec::new();
    let mut current = match *typ {
        Type::App(ref l, ref rest) => {
            // No need to unroll if `l` is not an application as that will just result in returning
            // an application that is identical to `typ`
            match **l {
                Type::App(..) => (),
                _ => return None,
            }
            args.extend(rest.iter().rev().cloned());
            l
        }
        _ => return unroll_record(typ),
    };
    while let Type::App(ref l, ref rest) = **current {
        args.extend(rest.iter().rev().cloned());
        current = l;
    }
    if args.is_empty() {
        None
    } else {
        args.reverse();
        Some(Type::app(current.clone(), args))
    }
}

fn unroll_record(typ: &Type<Symbol>) -> Option<ArcType> {
    let mut new_types = Vec::new();
    let mut new_fields = Vec::new();
    let mut current = match *typ {
        Type::ExtendRow { ref types, ref fields, ref rest } => {
            match **rest {
                Type::ExtendRow { .. } => {
                    new_types.extend_from_slice(types);
                    new_fields.extend_from_slice(fields);
                    rest
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    while let Type::ExtendRow { ref types, ref fields, ref rest } = **current {
        new_types.extend_from_slice(types);
        new_fields.extend_from_slice(fields);
        current = rest;
    }
    if new_types.is_empty() && new_fields.is_empty() {
        None
    } else {
        Some(Type::extend_row(new_types, new_fields, current.clone()))
    }
}
