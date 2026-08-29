use allen_bytecode::{
    Constant, Function, Instruction, ListLiteralItem, MapLiteralItem, Module, ValueType, verify,
};
use allen_vm::{ExecutionLimits, Value, VmError, execute, execute_with_limits};
use std::rc::Rc;

fn module(
    constants: Vec<Constant>,
    registers: Vec<ValueType>,
    return_type: ValueType,
    code: Vec<Instruction>,
) -> allen_bytecode::VerifiedModule {
    verify(Module {
        constants,
        enum_types: Vec::new(),
        effect_sets: vec![Vec::new()],
        functions: vec![Function {
            name: "main".into(),
            parameters: Vec::new(),
            parameter_names: Vec::new(),
            parameter_default_digests: Vec::new(),
            captures: Vec::new(),
            registers,
            return_type,
            effects: 0,
            code,
        }],
        async_functions: Vec::new(),
        entry: 0,
    })
    .expect("test module verifies")
}

#[test]
fn builds_list_spreads_in_source_order() {
    let list = ValueType::List(Box::new(ValueType::Int));
    let module = module(
        vec![Constant::Int(1), Constant::Int(2)],
        vec![ValueType::Int, ValueType::Int, list.clone(), list.clone()],
        list,
        vec![
            Instruction::Const {
                destination: 0,
                constant: 0,
            },
            Instruction::Const {
                destination: 1,
                constant: 1,
            },
            Instruction::ListNew {
                destination: 2,
                elements: vec![1],
            },
            Instruction::ListLiteralBuild {
                destination: 3,
                items: vec![ListLiteralItem::Element(0), ListLiteralItem::Spread(2)],
            },
            Instruction::Return { source: 3 },
        ],
    );
    assert_eq!(
        execute(&module),
        Ok(Value::List(Rc::from(
            [Value::Int(1), Value::Int(2)].as_slice()
        )))
    );
    assert_eq!(
        execute_with_limits(
            &module,
            ExecutionLimits {
                allocation_bytes: 159,
                ..ExecutionLimits::default()
            },
        ),
        Err(VmError::ResourceLimit {
            resource: "allocation_bytes"
        })
    );
}

#[test]
fn map_spreads_and_entries_use_last_write_wins() {
    let map = ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::Int));
    let module = module(
        vec![
            Constant::Int(1),
            Constant::Int(10),
            Constant::Int(2),
            Constant::Int(20),
        ],
        vec![
            ValueType::Int,
            ValueType::Int,
            ValueType::Int,
            ValueType::Int,
            map.clone(),
            map.clone(),
        ],
        map,
        vec![
            Instruction::Const {
                destination: 0,
                constant: 0,
            },
            Instruction::Const {
                destination: 1,
                constant: 1,
            },
            Instruction::Const {
                destination: 2,
                constant: 2,
            },
            Instruction::Const {
                destination: 3,
                constant: 3,
            },
            Instruction::MapNew {
                destination: 4,
                entries: vec![(0, 1)],
            },
            Instruction::MapLiteralBuild {
                destination: 5,
                items: vec![
                    MapLiteralItem::Entry { key: 0, value: 3 },
                    MapLiteralItem::Spread(4),
                    MapLiteralItem::Entry { key: 2, value: 3 },
                ],
            },
            Instruction::Return { source: 5 },
        ],
    );
    assert_eq!(
        execute(&module),
        Ok(Value::Map(Rc::from(
            [
                (Value::Int(1), Value::Int(10)),
                (Value::Int(2), Value::Int(20)),
            ]
            .as_slice(),
        )))
    );
}
