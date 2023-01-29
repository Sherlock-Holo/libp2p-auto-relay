fn main() {
    prost_build::compile_protos(&["proto/auto_relay.proto"], &["proto"]).unwrap()
}
