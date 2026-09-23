// Linux/x86_64 proposal module. No scheduler or guest RPC activation.
// Broker startup must precede guest/socket-pin creation. The run-global caller
// owns ReleaseService and stores ReleaseCustody under an authenticated key.
include!("credit-model.fragment.rs");
include!("service.fragment.rs");
include!("controller.fragment.rs");
include!("owner.fragment.rs");
include!("custody.fragment.rs");
