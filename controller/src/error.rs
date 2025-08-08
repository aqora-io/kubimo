use kubimo::kube::runtime;

pub type ControllerError<E> = runtime::controller::Error<E, runtime::watcher::Error>;
pub type ControllerResult<T, E> = Result<
    (
        runtime::reflector::ObjectRef<T>,
        runtime::controller::Action,
    ),
    ControllerError<E>,
>;
