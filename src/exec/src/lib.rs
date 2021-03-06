extern crate futures;
extern crate grpc;
extern crate grpc_protobuf;
extern crate protobuf;
#[macro_use]
extern crate log;

// Skip formatting the two modules because they are generated by grpc framework.
#[rustfmt::skip]
pub mod occlum_exec;
#[rustfmt::skip]
pub mod occlum_exec_grpc;

pub mod server;

pub const DEFAULT_SERVER_FILE: &'static str = "build/bin/occlum_exec_server";
pub const DEFAULT_SOCK_FILE: &'static str = "run/occlum_exec.sock";
pub const DEFAULT_SERVER_TIMER: u32 = 3;
