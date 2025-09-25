use kube::core::object::HasSpec;

pub trait ResourceFactory: HasSpec + Sized {
    fn new(name: &str, spec: Self::Spec) -> Self;
}
