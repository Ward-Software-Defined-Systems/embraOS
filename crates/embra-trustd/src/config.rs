use std::path::PathBuf;

#[derive(Clone)]
pub struct TrustdConfig {
    pub port: u16,
    pub state_dir: PathBuf,
    pub wardsondb_url: String,
}

impl TrustdConfig {
    pub fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut port = 50001u16;
        let mut state_dir = PathBuf::from("/embra/state");
        let mut wardsondb_url = "http://127.0.0.1:8090".to_string();

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--port" => { port = args[i+1].parse().expect("Invalid port"); i += 2; }
                "--state-dir" => { state_dir = PathBuf::from(&args[i+1]); i += 2; }
                "--wardsondb-url" => { wardsondb_url = args[i+1].clone(); i += 2; }
                _ => { i += 1; }
            }
        }

        Self { port, state_dir, wardsondb_url }
    }

    pub fn soul_hash_path(&self) -> PathBuf {
        self.state_dir.join("soul.sha256")
    }

    pub fn pki_dir(&self) -> PathBuf {
        self.state_dir.join("pki")
    }

    pub fn ca_cert_path(&self) -> PathBuf {
        self.pki_dir().join("ca.crt")
    }

    pub fn ca_key_path(&self) -> PathBuf {
        self.pki_dir().join("ca.key")
    }
}
