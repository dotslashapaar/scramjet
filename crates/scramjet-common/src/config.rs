use crate::error::ScramjetError;
use std::env;
use std::time::Duration;

/// Runtime configuration for Scramjet
/// Loaded from environment variables with sensible defaults
/// Validates all values on construction (fail-fast)
#[derive(Debug, Clone)]
pub struct Config {
    // --- Network Endpoints ---
    pub rpc_url: String,
    pub geyser_url: Option<String>,

    // --- Timing (Intervals in ms) ---
    pub rpc_poll_interval_ms: u64,
    pub scout_interval_ms: u64,
    pub scout_lookahead_slots: u64,
    pub monitor_interval_ms: u64,

    // --- Geyser Reconnection Backoff ---
    pub geyser_reconnect_delay_ms: u64,
    pub geyser_max_reconnect_delay_ms: u64,

    // --- QUIC Transport (in seconds) ---
    pub quic_keep_alive_secs: u64,
    pub quic_idle_timeout_secs: u64,

    // --- Transaction Defaults ---
    pub default_compute_unit_limit: u32,
    pub default_priority_fee: u64,
}

impl Config {
    /// Load configuration from environment variables
    /// Returns error if validation fails (fail-fast)
    pub fn from_env() -> Result<Self, ScramjetError> {
        let config = Self {
            // Network endpoints
            rpc_url: env::var("SOLANA_RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into()),
            geyser_url: env::var("GEYSER_URL").ok(),

            // Intervals
            rpc_poll_interval_ms: parse_env("RPC_POLL_INTERVAL_MS", 400),
            scout_interval_ms: parse_env("SCOUT_INTERVAL_MS", 1000),
            scout_lookahead_slots: parse_env("SCOUT_LOOKAHEAD_SLOTS", 10),
            monitor_interval_ms: parse_env("MONITOR_INTERVAL_MS", 400),

            // Backoff
            geyser_reconnect_delay_ms: parse_env("GEYSER_RECONNECT_DELAY_MS", 1000),
            geyser_max_reconnect_delay_ms: parse_env("GEYSER_MAX_RECONNECT_DELAY_MS", 10000),

            // QUIC
            quic_keep_alive_secs: parse_env("QUIC_KEEP_ALIVE_SECS", 5),
            quic_idle_timeout_secs: parse_env("QUIC_IDLE_TIMEOUT_SECS", 10),

            // Transaction
            default_compute_unit_limit: parse_env("DEFAULT_COMPUTE_UNIT_LIMIT", 200_000),
            default_priority_fee: parse_env("DEFAULT_PRIORITY_FEE", 100_000),
        };

        config.validate()?; // Fail-fast on invalid config
        Ok(config)
    }

    /// Validate configuration values (prevent runtime issues)
    fn validate(&self) -> Result<(), ScramjetError> {
        // Min interval to prevent CPU spike from tight loops
        const MIN_INTERVAL_MS: u64 = 50;

        if self.rpc_poll_interval_ms < MIN_INTERVAL_MS {
            return Err(ScramjetError::ConfigValidationError(format!(
                "RPC_POLL_INTERVAL_MS={} is too low (min {}ms). CPU will spike.",
                self.rpc_poll_interval_ms, MIN_INTERVAL_MS
            )));
        }

        if self.scout_interval_ms < MIN_INTERVAL_MS {
            return Err(ScramjetError::ConfigValidationError(format!(
                "SCOUT_INTERVAL_MS={} is too low (min {}ms). CPU will spike.",
                self.scout_interval_ms, MIN_INTERVAL_MS
            )));
        }

        if self.monitor_interval_ms < MIN_INTERVAL_MS {
            return Err(ScramjetError::ConfigValidationError(format!(
                "MONITOR_INTERVAL_MS={} is too low (min {}ms). CPU will spike.",
                self.monitor_interval_ms, MIN_INTERVAL_MS
            )));
        }

        // Compute unit limit must be > 0
        if self.default_compute_unit_limit == 0 {
            return Err(ScramjetError::ConfigValidationError(
                "DEFAULT_COMPUTE_UNIT_LIMIT=0 means all transactions will fail.".into(),
            ));
        }

        // QUIC idle timeout must be > 0
        if self.quic_idle_timeout_secs == 0 {
            return Err(ScramjetError::ConfigValidationError(
                "QUIC_IDLE_TIMEOUT_SECS=0 means connections disconnect immediately.".into(),
            ));
        }

        // Keep-alive must be less than idle timeout
        if self.quic_keep_alive_secs >= self.quic_idle_timeout_secs {
            return Err(ScramjetError::ConfigValidationError(format!(
                "QUIC_KEEP_ALIVE_SECS={} must be less than QUIC_IDLE_TIMEOUT_SECS={}.",
                self.quic_keep_alive_secs, self.quic_idle_timeout_secs
            )));
        }

        // Max backoff must be >= initial backoff
        if self.geyser_max_reconnect_delay_ms < self.geyser_reconnect_delay_ms {
            return Err(ScramjetError::ConfigValidationError(format!(
                "GEYSER_MAX_RECONNECT_DELAY_MS={} must be >= GEYSER_RECONNECT_DELAY_MS={}.",
                self.geyser_max_reconnect_delay_ms, self.geyser_reconnect_delay_ms
            )));
        }

        Ok(())
    }

    // --- Convenience methods returning Duration ---

    pub fn rpc_poll_interval(&self) -> Duration {
        Duration::from_millis(self.rpc_poll_interval_ms)
    }

    pub fn scout_interval(&self) -> Duration {
        Duration::from_millis(self.scout_interval_ms)
    }

    pub fn monitor_interval(&self) -> Duration {
        Duration::from_millis(self.monitor_interval_ms)
    }

    pub fn geyser_reconnect_delay(&self) -> Duration {
        Duration::from_millis(self.geyser_reconnect_delay_ms)
    }

    pub fn geyser_max_reconnect_delay(&self) -> Duration {
        Duration::from_millis(self.geyser_max_reconnect_delay_ms)
    }

    pub fn quic_keep_alive(&self) -> Duration {
        Duration::from_secs(self.quic_keep_alive_secs)
    }

    pub fn quic_idle_timeout(&self) -> Duration {
        Duration::from_secs(self.quic_idle_timeout_secs)
    }
}

/// Helper to parse env var with default fallback.
/// Logs a warning if the value exists but fails to parse.
fn parse_env<T: std::str::FromStr + std::fmt::Display>(key: &str, default: T) -> T {
    match env::var(key) {
        Ok(v) => match v.parse() {
            Ok(parsed) => parsed,
            Err(_) => {
                eprintln!(
                    "Warning: Invalid value for {}: '{}', using default {}",
                    key, v, default
                );
                default
            }
        },
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests must run serially since they modify env vars
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env_vars() {
        env::remove_var("SOLANA_RPC_URL");
        env::remove_var("GEYSER_URL");
        env::remove_var("RPC_POLL_INTERVAL_MS");
        env::remove_var("SCOUT_INTERVAL_MS");
        env::remove_var("MONITOR_INTERVAL_MS");
        env::remove_var("DEFAULT_COMPUTE_UNIT_LIMIT");
        env::remove_var("QUIC_KEEP_ALIVE_SECS");
        env::remove_var("QUIC_IDLE_TIMEOUT_SECS");
        env::remove_var("GEYSER_RECONNECT_DELAY_MS");
        env::remove_var("GEYSER_MAX_RECONNECT_DELAY_MS");
    }

    #[test]
    fn test_config_defaults() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_env_vars();

        let config = Config::from_env().expect("Default config should be valid");

        assert_eq!(config.rpc_url, "https://api.mainnet-beta.solana.com");
        assert!(config.geyser_url.is_none());
        assert_eq!(config.rpc_poll_interval_ms, 400);
        assert_eq!(config.scout_interval_ms, 1000);
        assert_eq!(config.default_compute_unit_limit, 200_000);
    }

    #[test]
    fn test_config_validation_interval_too_low() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_env_vars();

        env::set_var("RPC_POLL_INTERVAL_MS", "10");
        let result = Config::from_env();
        env::remove_var("RPC_POLL_INTERVAL_MS");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("too low"));
    }

    #[test]
    fn test_config_validation_zero_compute_units() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_env_vars();

        env::set_var("DEFAULT_COMPUTE_UNIT_LIMIT", "0");
        let result = Config::from_env();
        env::remove_var("DEFAULT_COMPUTE_UNIT_LIMIT");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("transactions will fail"));
    }

    #[test]
    fn test_config_validation_keep_alive_exceeds_timeout() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_env_vars();

        env::set_var("QUIC_KEEP_ALIVE_SECS", "15");
        env::set_var("QUIC_IDLE_TIMEOUT_SECS", "10");
        let result = Config::from_env();
        env::remove_var("QUIC_KEEP_ALIVE_SECS");
        env::remove_var("QUIC_IDLE_TIMEOUT_SECS");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must be less than"));
    }
}
