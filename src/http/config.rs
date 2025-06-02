use std::time::Duration;

pub struct Config {
    pub port: u16,
    pub request_timeout: Duration,
    pub connection_timeout: Duration,
    pub max_connections: usize,
    pub keep_alive_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { 
            port: 18000,
            request_timeout: Duration::from_secs(30),
            connection_timeout: Duration::from_secs(60),
            max_connections: 100,
            keep_alive_timeout: Duration::from_secs(75),
        }
    }
}

impl Config {
    pub fn new(port: u16) -> Self {
        Self { 
            port,
            ..Default::default()
        }
    }
    
    pub fn with_timeouts(port: u16, request_timeout: Duration, connection_timeout: Duration) -> Self {
        Self {
            port,
            request_timeout,
            connection_timeout,
            ..Default::default()
        }
    }
}
