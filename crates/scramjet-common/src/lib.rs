pub mod config;
pub mod error;
pub mod identity;

pub use config::Config;
pub use error::ScramjetError;
pub use identity::create_quic_config;

// --- UNIT TEST ---
#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::{Keypair, Signer};

    #[test]
    fn test_identity_generation() {
        // 1. Create a dummy Keypair
        let keypair = Keypair::new();
        println!("Testing with Pubkey: {}", keypair.pubkey());

        // 2. Load default config
        let config = Config::from_env().expect("Failed to load config");

        // 3. Generate the Config
        let config_result = create_quic_config(&keypair, &config);

        // 4. Assertion: If this passes, the crypto/cert logic is valid.
        assert!(
            config_result.is_ok(),
            "Failed to generate config: {:?}",
            config_result.err()
        );

        println!("Certificate generated successfully.");
    }
}
