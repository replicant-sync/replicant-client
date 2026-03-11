use replicant_client::events::SyncEvent;
use replicant_client::{Client, ClientDatabase};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// Shared state for tracking received events
struct EventTracker {
    events: Vec<String>,
}

impl EventTracker {
    fn new() -> Self {
        Self { events: Vec::new() }
    }

    fn add(&mut self, event: String) {
        println!("  📥 Callback received: {}", event);
        self.events.push(event);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt().with_env_filter("warn").init();

    println!("🧪 Testing Rust event callbacks...");

    // Setup database
    std::fs::create_dir_all("databases")?;
    let db_file = "databases/callback_test.sqlite3";
    let db_url = format!("sqlite:{}?mode=rwc", db_file);

    let db = Arc::new(ClientDatabase::new(&db_url).await?);
    db.run_migrations().await?;

    // Get or create user
    let user_id = match db.get_user_id().await {
        Ok(id) => id,
        Err(_) => {
            let id = Uuid::new_v4();
            let client_id = Uuid::new_v4();
            setup_user(&db, id, client_id, "ws://nonexistent:8080/ws", "test-token").await?;
            id
        }
    };

    println!("👤 User ID: {}", user_id);

    // Shared state for event tracking (thread-safe)
    let tracker = Arc::new(Mutex::new(EventTracker::new()));

    // Try to connect to server (will fail, but we want to test offline mode)
    let sync_engine = match Client::new(
        &db_url,
        "ws://nonexistent:8080/ws",
        "test-user@example.com",
        "test-key",
        "test-secret",
    )
    .await
    {
        Ok(engine) => {
            println!("📡 Sync engine created successfully");

            // Register Rust-native callback using the new SyncEvent enum
            let tracker_clone = tracker.clone();
            engine
                .event_dispatcher()
                .register_rust_callback(move |event| {
                    let event_desc = match &event {
                        SyncEvent::DocumentCreated { id, title, .. } => {
                            format!("📄 Document created: {} ({})", title, &id[..8])
                        }
                        SyncEvent::DocumentUpdated { id, title, .. } => {
                            format!("✏️ Document updated: {} ({})", title, &id[..8])
                        }
                        SyncEvent::DocumentDeleted { id } => {
                            format!("🗑️ Document deleted: {}", &id[..8])
                        }
                        SyncEvent::SyncStarted => "🔄 Sync started".to_string(),
                        SyncEvent::SyncCompleted { document_count } => {
                            format!("✅ Sync completed: {} docs", document_count)
                        }
                        SyncEvent::SyncError { message } => {
                            format!("🚨 Sync error: {}", message)
                        }
                        SyncEvent::ConnectionLost { server_url } => {
                            format!("❌ Disconnected from {}", server_url)
                        }
                        SyncEvent::ConnectionAttempted { server_url } => {
                            format!("🔄 Connecting to {}...", server_url)
                        }
                        SyncEvent::ConnectionSucceeded { server_url } => {
                            format!("🔗 Connected to {}", server_url)
                        }
                        SyncEvent::ConflictDetected { document_id, .. } => {
                            format!("⚠️ Conflict detected: {}", &document_id[..8])
                        }
                    };

                    if let Ok(mut t) = tracker_clone.lock() {
                        t.add(event_desc);
                    }
                })?;

            println!("✓ Rust callback registered");
            Some(Arc::new(engine))
        }
        Err(e) => {
            println!("⚠️ Offline mode: {}", e);
            None
        }
    };

    // Test local document operations
    println!("\n🔧 Testing document operations...");

    // Create some test documents
    for i in 1..=3 {
        let title = format!("Test Task {}", i);
        let content = json!({
            "title": title.clone(),
            "description": format!("This is test task number {}", i),
            "status": "pending",
            "priority": if i == 1 { "high" } else { "medium" },
            "tags": ["test", "demo"]
        });

        if let Some(engine) = &sync_engine {
            println!("  ➕ Creating document: {}", title);
            let mut full_content = content.clone();
            full_content["title"] = serde_json::json!(title);
            let doc = engine.create_document(full_content).await?;
            println!("     Created: {}", doc.id);
        } else {
            // Offline mode - create directly in database
            println!("  ➕ Creating document offline: {}", title);
            let mut full_content = content.clone();
            full_content["title"] = serde_json::json!(title);
            let doc = replicant_core::models::Document {
                id: Uuid::new_v4(),
                user_id: Some(user_id),
                content: full_content.clone(),
                sync_revision: 1,
                content_hash: None,
                title: full_content
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                deleted_at: None,
            };

            db.save_document(&doc).await?;
            println!("     Created: {}", doc.id);
        }

        // Give time for async operations
        sleep(Duration::from_millis(10)).await;
    }

    // Test event emission with callback processing
    println!("\n🧪 Testing event emission...");
    if let Some(engine) = &sync_engine {
        let events = engine.event_dispatcher();

        // Emit some test events
        let test_doc_id = Uuid::new_v4();
        let test_content = json!({"title": "Test Document", "test": "data"});

        println!("  📤 Emitting test events...");
        events.emit_document_created(&test_doc_id, &test_content);
        events.emit_sync_started();
        events.emit_sync_completed(42);
        events.emit_connection_succeeded("ws://test-server");

        // Process queued events - this invokes our Rust callback
        let processed = events.process_events()?;
        println!("  🔄 Processed {} events", processed);

        // Check connection status
        println!(
            "  🔗 Connection status: {}",
            if engine.is_connected() {
                "Connected"
            } else {
                "Disconnected"
            }
        );
    }

    // Wait a bit more
    sleep(Duration::from_millis(100)).await;

    // Show summary of received events
    println!("\n📊 Event Summary:");
    if let Ok(t) = tracker.lock() {
        println!("   Total events received: {}", t.events.len());
        for (i, event) in t.events.iter().enumerate() {
            println!("   {}. {}", i + 1, event);
        }
    }

    println!("\n✅ Rust callback test completed!");
    Ok(())
}

async fn setup_user(
    db: &ClientDatabase,
    user_id: Uuid,
    client_id: Uuid,
    server_url: &str,
    _token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO user_config (user_id, client_id, server_url) VALUES (?1, ?2, ?3)")
        .bind(user_id.to_string())
        .bind(client_id.to_string())
        .bind(server_url)
        .execute(&db.pool)
        .await?;
    Ok(())
}
