//! Spike test for phoenix_channels_client against Elixir/Phoenix server
//!
//! Run with:
//!   1. Start Phoenix server: cd replicant_server && mix phx.server
//!   2. Generate credentials: mix replicant.gen.credentials --name "spike"
//!   3. Run spike:
//!      REPLICANT_API_KEY="rpa_..." REPLICANT_API_SECRET="rps_..." \
//!      cargo run --example phoenix_spike
//!
//! Pass criteria:
//!   - Clean connect/join/leave lifecycle
//!   - Request-reply works
//!   - Error handling works

use phoenix_channels_client::{Event, Payload, Socket, Topic};
use serde_json::json;
use std::time::Duration;
use url::Url;

// Test credentials - set via environment variables
// Generate with: mix replicant.gen.credentials --name "Spike Test"
fn get_api_key() -> String {
    std::env::var("REPLICANT_API_KEY").expect("REPLICANT_API_KEY env var required")
}
fn get_secret() -> String {
    std::env::var("REPLICANT_API_SECRET").expect("REPLICANT_API_SECRET env var required")
}
fn get_email() -> String {
    std::env::var("REPLICANT_EMAIL").unwrap_or_else(|_| "spike@test.com".to_string())
}
fn get_server_url() -> String {
    std::env::var("SYNC_SERVER_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:4000/socket/websocket".to_string())
}

fn create_signature(secret: &str, timestamp: i64, email: &str, api_key: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let message = format!("{}.{}.{}.", timestamp, email, api_key);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .is_test(true)
        .try_init();

    println!("=== Phoenix Channels Client Spike Test ===\n");

    let server_url = get_server_url();
    let api_key = get_api_key();
    let secret = get_secret();
    let email = get_email();

    // Test 1: Connect to server
    println!("1. Connecting to Phoenix server at {}...", server_url);
    let url = Url::parse(&server_url)?;
    let socket = Socket::spawn(url, None, None).await?;
    socket.connect(Duration::from_secs(10)).await?;
    println!("   ✓ Connected\n");

    // Test 2: Join channel with authentication
    println!("2. Joining sync channel with HMAC auth...");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let signature = create_signature(&secret, timestamp, &email, &api_key);

    let join_payload = json!({
        "email": email,
        "api_key": api_key,
        "signature": signature,
        "timestamp": timestamp
    });

    let payload = Payload::json_from_serialized(join_payload.to_string())?;
    let channel = socket
        .channel(Topic::from_string("sync:main".to_string()), Some(payload))
        .await?;

    channel.join(Duration::from_secs(10)).await?;
    println!("   ✓ Joined channel\n");

    // Test 3: Create a document
    println!("3. Creating a document...");
    let doc_id = uuid::Uuid::new_v4().to_string();
    let create_payload = json!({
        "id": doc_id,
        "content": {"title": "Spike Test Document", "body": "Hello from Rust!"}
    });

    let response = channel
        .call(
            Event::from_string("create_document".to_string()),
            Payload::json_from_serialized(create_payload.to_string())?,
            Duration::from_secs(5),
        )
        .await;
    match response {
        Ok(reply) => println!("   Response: {:?}", reply),
        Err(e) => println!("   Error: {:?}", e),
    }
    println!("   ✓ Create document sent\n");

    // Test 4: Request full sync
    println!("4. Requesting full sync...");
    let response = channel
        .call(
            Event::from_string("request_full_sync".to_string()),
            Payload::json_from_serialized(json!({}).to_string())?,
            Duration::from_secs(5),
        )
        .await;
    match response {
        Ok(reply) => println!("   Response: {:?}", reply),
        Err(e) => println!("   Error: {:?}", e),
    }
    println!("   ✓ Full sync completed\n");

    // Test 5: Get changes since
    println!("5. Getting changes since sequence 0...");
    let response = channel
        .call(
            Event::from_string("get_changes_since".to_string()),
            Payload::json_from_serialized(json!({"last_sequence": 0}).to_string())?,
            Duration::from_secs(5),
        )
        .await;
    match response {
        Ok(reply) => println!("   Response: {:?}", reply),
        Err(e) => println!("   Error: {:?}", e),
    }
    println!("   ✓ Changes retrieved\n");

    // Test 6: Leave channel
    println!("6. Leaving channel...");
    channel.leave().await?;
    println!("   ✓ Left channel\n");

    // Test 7: Disconnect
    println!("7. Disconnecting...");
    socket.disconnect().await?;
    println!("   ✓ Disconnected\n");

    println!("=== Spike test completed ===");
    println!("\nIf all tests showed responses, phoenix_channels_client works correctly with our Phoenix server.");
    println!("Proceed with Phase 6b: Rust client websocket rewrite using phoenix_channels_client.");

    Ok(())
}
