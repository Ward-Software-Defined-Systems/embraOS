//! Configuration for embra-brain service.

#[derive(Clone)]
pub struct BrainConfig {
    pub port: u16,
    pub wardsondb_url: String,
}

impl BrainConfig {
    pub fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut port = 50002u16;
        let mut wardsondb_url = "http://127.0.0.1:8090".to_string();

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--port" => { port = args[i+1].parse().expect("Invalid port"); i += 2; }
                "--wardsondb-url" => { wardsondb_url = args[i+1].clone(); i += 2; }
                _ => { i += 1; }
            }
        }

        Self { port, wardsondb_url }
    }
}
