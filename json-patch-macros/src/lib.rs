#[macro_export]
macro_rules! path {
    () => {
        ::json_patch::jsonptr::PointerBuf::root()
    };
    ($($token:expr),*$(,)?) => {
        ::json_patch::jsonptr::PointerBuf::from_tokens([
            $(::json_patch::jsonptr::Token::from($token)),*
        ])
    };
}

#[macro_export]
macro_rules! add {
    ([$($path:expr),*$(,)?] => $($json:tt)*) => {
        ::json_patch::PatchOperation::Add(
            ::json_patch::AddOperation {
                path: path!($($path),*),
                value: ::serde_json::json!($($json)*),
            }
        )
    };
}

#[macro_export]
macro_rules! rm {
    ([$($path:expr),*$(,)?]) => {
        ::json_patch::PatchOperation::Remove(
            ::json_patch::RemoveOperation {
                path: path!($($path),*),
            }
        )
    };
}

#[macro_export]
macro_rules! put {
    ([$($path:expr),*$(,)?] => $($json:tt)*) => {
        ::json_patch::PatchOperation::Replace(
            ::json_patch::ReplaceOperation {
                path: path!($($path),*),
                value: ::serde_json::json!($($json)*),
            }
        )
    };
}

#[macro_export]
macro_rules! mv {
    ([$($from:expr),*$(,)?] => [$($to:expr),*$(,)?]) => {
        ::json_patch::PatchOperation::Move(
            ::json_patch::MoveOperation {
                from: path!($($from),*),
                path: path!($($to),*),
            }
        )
    };
}

#[macro_export]
macro_rules! cp {
    ([$($from:expr),*$(,)?] => [$($to:expr),*$(,)?]) => {
        ::json_patch::PatchOperation::Copy(
            ::json_patch::CopyOperation {
                from: path!($($from),*),
                path: path!($($to),*),
            }
        )
    };
}

#[macro_export]
macro_rules! test {
    ([$($path:expr),*$(,)?] => $($json:tt)*) => {
        ::json_patch::PatchOperation::Test(
            ::json_patch::TestOperation {
                path: path!($($path),*),
                value: ::serde_json::json!($($json)*),
            }
        )
    };
}

#[macro_export]
macro_rules! patch {
    ($($op:expr),*$(,)?) => {
        ::json_patch::Patch(vec![$($op),*])
    };
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    #[derive(Serialize, Debug)]
    pub struct TestStruct {
        hello: String,
    }

    #[test]
    fn test_ops() {
        assert_eq!(
            patch!(
                add!(["cool", "hello", 0, "world"] => TestStruct { hello: "world".to_string() }),
                rm!(["bad", "item"]),
                put!([] => null),
                mv!(["from"] => ["to"]),
                cp!(["from".to_string()] => ["to".to_string()]),
                test!([0] => true),
            ),
            json_patch::Patch(vec![
                json_patch::PatchOperation::Add(json_patch::AddOperation {
                    path: json_patch::jsonptr::Pointer::from_static("/cool/hello/0/world")
                        .to_owned(),
                    value: serde_json::json!({"hello": "world"}),
                }),
                json_patch::PatchOperation::Remove(json_patch::RemoveOperation {
                    path: json_patch::jsonptr::Pointer::from_static("/bad/item").to_owned(),
                }),
                json_patch::PatchOperation::Replace(json_patch::ReplaceOperation {
                    path: json_patch::jsonptr::PointerBuf::root(),
                    value: serde_json::json!(null),
                }),
                json_patch::PatchOperation::Move(json_patch::MoveOperation {
                    from: json_patch::jsonptr::Pointer::from_static("/from").to_owned(),
                    path: json_patch::jsonptr::Pointer::from_static("/to").to_owned(),
                }),
                json_patch::PatchOperation::Copy(json_patch::CopyOperation {
                    from: json_patch::jsonptr::Pointer::from_static("/from").to_owned(),
                    path: json_patch::jsonptr::Pointer::from_static("/to").to_owned(),
                }),
                json_patch::PatchOperation::Test(json_patch::TestOperation {
                    path: json_patch::jsonptr::Pointer::from_static("/0").to_owned(),
                    value: serde_json::json!(true),
                }),
            ])
        );
    }
}
