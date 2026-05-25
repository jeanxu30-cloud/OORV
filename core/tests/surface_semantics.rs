use oorv_core::oorvir::refined::{AccessMode, ExprVariant, Shift, StreamIdx, OORVIR};
use oorv_core::oorvir::source::pipeline::OORVAstParser;
use oorv_core::parse::OORVSpecParser;

fn compile_refined(spec: &str) -> OORVIR {
    let ast = OORVSpecParser::parse_for_ast(spec.to_string(), "<test>".to_string())
        .expect("spec should parse");
    let source_ir = OORVAstParser::parse_for_ir(ast, "<test>".to_string())
        .expect("spec should lower to source IR");
    OORVIR::compile_from_source(source_ir)
}

fn contains_shift(expr: &oorv_core::oorvir::refined::Expression, expected: u16) -> bool {
    match &expr.kind {
        ExprVariant::StreamAccess {
            access_kind: AccessMode::Shift(offset),
            ..
        } => *offset == Shift::Past(expected.into()),
        ExprVariant::StreamAccess { parameters, .. } => parameters
            .iter()
            .any(|param| contains_shift(param, expected)),
        ExprVariant::ArithLog(_, operands) => operands
            .iter()
            .any(|operand| contains_shift(operand, expected)),
        ExprVariant::Ite {
            condition,
            consequence,
            alternative,
        } => {
            contains_shift(condition, expected)
                || contains_shift(consequence, expected)
                || contains_shift(alternative, expected)
        }
        ExprVariant::Tuple(items) => items.iter().any(|item| contains_shift(item, expected)),
        ExprVariant::TupleAccess(inner, _) => contains_shift(inner, expected),
        ExprVariant::Function(_, args) => args.iter().any(|arg| contains_shift(arg, expected)),
        ExprVariant::Convert { expr } => contains_shift(expr, expected),
        ExprVariant::Default { expr, default } => {
            contains_shift(expr, expected) || contains_shift(default, expected)
        }
        ExprVariant::Quantified { body, .. } => contains_shift(body, expected),
        ExprVariant::LoadConstant(_)
        | ExprVariant::ParameterAccess(_, _)
        | ExprVariant::FunctionParameterAccess(_)
        | ExprVariant::QuantifiedVar(_) => false,
    }
}

#[test]
fn parser_accepts_preferred_surface_aliases() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        all_nonnegative @always {
            if forall car in cars: car.speed.last(default:0) >= 0 {
                info! "all cars have nonnegative speed";
            }
        }
}
"#;

    let ir = compile_refined(spec);
    assert_eq!(ir.signals.len(), 2, "uid and speed should be input streams");
    assert!(
        !ir.alarms.is_empty(),
        "constraint should compile as an alarm stream"
    );
}

#[test]
fn quantified_preprocessing_preserves_named_domains() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        has_moving_car @always {
            if exists car in cars: car.speed.last(default:0) > 0 {
                alert! "moving car";
            }
        }
}
"#;

    let ir = compile_refined(spec);
    let alarm = ir
        .constraints
        .iter()
        .find(|constraint| matches!(constraint.stream_idx, StreamIdx::Constraint(_)))
        .expect("alarm constraint should be present");
    let guard = alarm.eval.decls[0]
        .condition
        .as_ref()
        .expect("alarm should retain the quantified guard");

    match &guard.kind {
        ExprVariant::Quantified {
            bindings1,
            bindings2,
            ..
        } => {
            assert_eq!(bindings1, &vec!["car".to_string()]);
            assert_eq!(bindings2, &vec!["cars".to_string()]);
        }
        other => panic!("expected quantified guard, got {other:?}"),
    }
}

#[test]
fn world_collection_domains_lower_to_active_instance_streams() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        has_car @always {
            if exists car in cars: true {
                alert! "car exists";
            }
        }
}
"#;

    let ir = compile_refined(spec);
    let domain_stream = ir
        .object_domains
        .get("cars")
        .expect("world collection `cars` should map to an active-instance stream");
    let stream = &ir.constraints[domain_stream.out_ix()];
    assert_eq!(
        stream.name, "Vehicle::Car::speed_params",
        "world collection should use a representative parameterized stream for Vehicle::Car"
    );
}

#[test]
fn world_constraints_accept_history_access() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        speed_regression @always {
            if exists car in cars:
                car.speed.history(at:-2, default:0) > car.speed.prev(default:0) {
                alert! "speed regressed";
            }
        }
}
"#;

    let ir = compile_refined(spec);
    let guard = ir.constraints[0]
        .eval
        .decls
        .first()
        .and_then(|decl| decl.condition.as_ref())
        .expect("history example should compile to a guarded alarm");

    assert!(
        contains_shift(guard, 2),
        "world-level history(at:-2, default:...) should lower to a two-step shift"
    );
}
