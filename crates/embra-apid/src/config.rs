#[derive(Clone)]
pub struct ApidConfig {
    pub grpc_port: u16,
    pub rest_port: u16,
    pub brain_addr: String,
    pub trust_addr: String,
}

impl ApidConfig {
    pub fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut grpc_port = 50000u16;
        let mut rest_port = 8443u16;
        let mut brain_addr = "http://127.0.0.1:50002".to_string();
        let mut trust_addr = "http://127.0.0.1:50001".to_string();

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--grpc-port" => { grpc_port = args[i+1].parse().expect("Invalid gRPC port"); i += 2; }
                "--rest-port" => { rest_port = args[i+1].parse().expect("Invalid REST port"); i += 2; }
                "--brain-addr" => { brain_addr = args[i+1].clone(); i += 2; }
                "--trust-addr" => { trust_addr = args[i+1].clone(); i += 2; }
                _ => { i += 1; }
            }
        }

        Self { grpc_port, rest_port, brain_addr, trust_addr }
    }
}
