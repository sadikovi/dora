use std::sync::{Arc, Mutex};

use ast;
use ast::visit::{self, Visitor};
use class::*;
use ctxt::{Context, Fct, FctId, FctKind, FctSrc, StructFieldData, StructId};
use error::msg::Msg;
use interner::Name;
use lexer::position::Position;
use mem;
use semck;
use sym::Sym;
use ty::BuiltinType;

pub fn check<'ast>(ctxt: &mut Context<'ast>) {
    let mut clsck = ClsDefCheck {
        ctxt: ctxt,
        ast: ctxt.ast,
        cls_id: None,
        struct_id: None,
    };

    clsck.check();
}

struct ClsDefCheck<'x, 'ast: 'x> {
    ctxt: &'x mut Context<'ast>,
    ast: &'ast ast::Ast,
    cls_id: Option<ClassId>,
    struct_id: Option<StructId>,
}

impl<'x, 'ast> ClsDefCheck<'x, 'ast> {
    fn check(&mut self) {
        self.visit_ast(self.ast);
    }

    fn cls(&self) -> &Class<'ast> {
        self.ctxt.cls_by_id(self.cls_id.unwrap())
    }

    fn cls_mut(&mut self) -> &mut Class<'ast> {
        self.ctxt.cls_by_id_mut(self.cls_id.unwrap())
    }

    fn add_field(&mut self, pos: Position, name: Name, ty: BuiltinType, reassignable: bool) {
        for field in &self.cls().fields {
            if field.name == name {
                let name = self.ctxt.interner.str(name).to_string();
                report(self.ctxt, pos, Msg::ShadowField(name));
            }
        }

        let mut class = self.cls_mut();

        let offset = if ty.size() > 0 {
            mem::align_i32(class.size, ty.size())
        } else {
            class.size
        };

        let field = Field {
            id: class.fields.len().into(),
            name: name,
            ty: ty,
            offset: offset,
            reassignable: reassignable,
        };

        class.size = offset + ty.size();
        class.fields.push(field);
    }
}

impl<'x, 'ast> Visitor<'ast> for ClsDefCheck<'x, 'ast> {
    fn visit_class(&mut self, c: &'ast ast::Class) {
        self.cls_id = Some(*self.ctxt.map_cls_defs.get(c.id).unwrap());

        visit::walk_class(self, c);

        if let Some(ref parent_class) = c.parent_class {
            let name = self.ctxt.interner.str(parent_class.name).to_string();
            let sym = self.ctxt.sym.borrow().get(parent_class.name);

            match sym {
                Some(Sym::SymClass(clsid)) => {
                    let has_open = self.ctxt.cls_by_id(clsid).has_open;

                    if has_open {
                        self.cls_mut().parent_class = Some(clsid);
                    } else {
                        let msg = Msg::UnderivableType(name);
                        self.ctxt.diag.borrow_mut().report(parent_class.pos, msg);
                    }
                }

                _ => {
                    let msg = Msg::UnknownClass(name);
                    self.ctxt.diag.borrow_mut().report(parent_class.pos, msg);
                }
            };
        }

        self.cls_id = None;
    }

    fn visit_struct(&mut self, s: &'ast ast::Struct) {
        self.struct_id = Some(*self.ctxt.map_struct_defs.get(s.id).unwrap());

        visit::walk_struct(self, s);

        self.struct_id = None;
    }

    fn visit_struct_field(&mut self, f: &'ast ast::StructField) {
        let ty = semck::read_type(self.ctxt, &f.data_type).unwrap_or(BuiltinType::Unit);
        let id = self.struct_id.unwrap();

        {
            let struc = self.ctxt.struct_by_id(id);

            for field in &struc.fields {
                if field.name == f.name {
                    let name = self.ctxt.interner.str(f.name).to_string();
                    report(self.ctxt, f.pos, Msg::ShadowField(name));
                    return;
                }
            }
        }

        let mut struc = self.ctxt.struct_by_id_mut(id);

        let field = StructFieldData {
            id: (struc.fields.len() as u32).into(),
            name: f.name,
            ty: ty,
            offset: 0,
        };

        struc.fields.push(field);
    }

    fn visit_field(&mut self, f: &'ast ast::Field) {
        let ty = semck::read_type(self.ctxt, &f.data_type).unwrap_or(BuiltinType::Unit);
        self.add_field(f.pos, f.name, ty, f.reassignable);

        if !f.reassignable && !f.primary_ctor && f.expr.is_none() {
            self.ctxt.diag.borrow_mut().report(f.pos, Msg::LetMissingInitialization);
        }
    }

    fn visit_ctor(&mut self, f: &'ast ast::Function) {
        let clsid = self.cls_id.unwrap();

        let fct = Fct {
            id: FctId(0),
            pos: f.pos,
            ast: f,
            name: f.name,
            params_types: Vec::new(),
            return_type: BuiltinType::Unit,
            owner_class: Some(clsid),
            has_override: f.has_override,
            has_open: f.has_open,
            has_final: f.has_final,
            internal: f.internal,
            overrides: None,
            throws: f.throws,
            ctor: f.ctor,
            vtable_index: None,
            initialized: false,
            kind: FctKind::Source(Arc::new(Mutex::new(FctSrc::new()))),
        };

        let fctid = self.ctxt.add_fct(fct);
        self.cls_mut().ctors.push(fctid);
    }

    fn visit_method(&mut self, f: &'ast ast::Function) {
        let kind = if f.block.is_some() {
            FctKind::Source(Arc::new(Mutex::new(FctSrc::new())))
        } else {
            FctKind::Definition
        };

        let fct = Fct {
            id: FctId(0),
            ast: f,
            pos: f.pos,
            name: f.name,
            params_types: Vec::new(),
            return_type: BuiltinType::Unit,
            owner_class: Some(self.cls_id.unwrap()),
            has_override: f.has_override,
            has_open: f.has_open,
            has_final: f.has_final,
            internal: f.internal,
            overrides: None,
            throws: f.throws,
            ctor: ast::CtorType::None,
            vtable_index: None,
            initialized: false,
            kind: kind,
        };

        let fctid = self.ctxt.add_fct(fct);

        self.cls_mut().methods.push(fctid);
    }
}

fn report(ctxt: &Context, pos: Position, msg: Msg) {
    ctxt.diag.borrow_mut().report(pos, msg);
}

#[cfg(test)]
mod tests {
    use error::msg::Msg;
    use mem;
    use object::Header;
    use semck::tests::*;

    fn class_size(code: &'static str) -> i32 {
        ok_with_test(code, |ctxt| ctxt.classes[4].size)
    }

    #[test]
    fn test_class_size() {
        assert_eq!(Header::size(), class_size("class Foo"));
        assert_eq!(Header::size() + 4, class_size("class Foo(let a: int)"));
        assert_eq!(Header::size() + mem::ptr_width(),
                   class_size("class Foo(let a: Str)"));
    }

    #[test]
    fn test_multiple_definition() {
        err("class Foo class Foo",
            pos(1, 11),
            Msg::ShadowClass("Foo".into()));
    }

    #[test]
    fn test_class_and_function() {
        err("fun Foo() {} class Foo",
            pos(1, 14),
            Msg::ShadowFunction("Foo".into()));
        err("class Foo fun Foo() {}",
            pos(1, 11),
            Msg::ShadowClass("Foo".into()));
    }

    #[test]
    fn test_class_definition() {
        ok("class Foo");
        ok("class Foo()");
        ok("class Foo(let a: int)");
        ok("class Foo(let a: int, let b:int)");
        ok("class Foo(let a: Foo)");
        ok("class Foo(let a: Bar) class Bar");
        err("class Foo(let a: Unknown)",
            pos(1, 18),
            Msg::UnknownType("Unknown".into()));
        err("class Foo(let a: int, let a: int)",
            pos(1, 27),
            Msg::ShadowField("a".to_string()));
    }

    #[test]
    fn class_with_unknown_super_class() {
        err("class B : A {}", pos(1, 11), Msg::UnknownClass("A".into()));
        err("open class B : A {}",
            pos(1, 16),
            Msg::UnknownClass("A".into()));
        err("class B : int {}",
            pos(1, 11),
            Msg::UnderivableType("int".into()));
    }

    #[test]
    fn class_with_open_modifier() {
        ok("open class A {}");
        ok("open class A {} class B : A {}");
        err("class A {} class B : A {}",
            pos(1, 22),
            Msg::UnderivableType("A".into()));
    }

    #[test]
    fn non_field_ctor_arguments() {
        ok("class Foo(a: int, b: int)");
        ok("class Foo(let a: int, b: int)");
        ok("class Foo(a: int, var b: int)");
        err("class Foo(a: int, a: int)",
            pos(1, 1),
            Msg::ShadowParam("a".into()));
        err("class Foo(a: int, let a: int)",
            pos(1, 1),
            Msg::ShadowParam("a".into()));
        err("class Foo(let a: int, a: int)",
            pos(1, 1),
            Msg::ShadowParam("a".into()));
        err("class Foo(a: int) fun f(x: Foo) { x.a = 1; }",
            pos(1, 36),
            Msg::UnknownField("a".into(), "Foo".into()));

        ok("class Foo(a: int) fun foo() -> Foo { return Foo(1); } ");
    }

    #[test]
    fn field_defined_twice() {
        err("class Foo { var a: int; var a: int; }",
            pos(1, 25),
            Msg::ShadowField("a".into()));
        err("class Foo(let a: int) { var a: int; }",
            pos(1, 25),
            Msg::ShadowField("a".into()));

    }

    #[test]
    fn let_field_without_initialization() {
        err("class Foo { let a: int; }",
            pos(1, 13),
            Msg::LetMissingInitialization);
    }

    #[test]
    fn field_self_assignment() {
        err("class Foo(a: int) { var b: int = b; }",
            pos(1, 34),
            Msg::UnknownIdentifier("b".into()));
    }

    #[test]
    fn struct_field() {
        ok("struct Foo { a: int }");
        ok("struct Foo { a: int, b: int }");
        ok("struct Foo { a: int } struct Bar { a: int }");
        ok("struct Foo { a: int, bar: Bar } struct Bar { a: int }");
        err("struct Bar { a: Unknown }",
            pos(1, 17),
            Msg::UnknownType("Unknown".into()));
        err("struct Foo { a: int, a: int }",
            pos(1, 22),
            Msg::ShadowField("a".into()));
    }
}
