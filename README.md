# JSON Database Sync

A client-server synchronization system built in Rust, featuring real-time WebSocket communication, bidirectional patch-based version control, and automatic conflict resolution.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

## Features

### Real-Time Synchronization
- **WebSocket-based** bidirectional sync
- **Basic conflict resolution** with server-wins fallback
- **Offline-first** design with local storage
- **Concurrent update handling** with conflict detection

### Version Control
- **JSON patches**: Forward patches for document changes
- **Document versioning**: Each document has version and revision tracking
- **Change events**: All modifications logged with sequence numbers
- **Audit trails**: Changes logged with user attribution

### Database Architecture
- **Client**: SQLite with offline queue and document caching
- **Server**: PostgreSQL with JSONB support and event logging
- **Change events**: Sequence-based sync with forward/reverse patch storage

## Architecture

- **sync-core**: Shared library with data models, JSON patch operations, and sync protocols
- **sync-client**: Client library with SQLite storage, offline queue, and C FFI exports
- **sync-server**: Server binary with PostgreSQL storage, WebSocket support, and event logging

## Quick Start

### Prerequisites
- Rust 1.75+
- PostgreSQL 16+
- Docker (optional)

### Option 1: Docker (Recommended)

```bash
git clone https://github.com/yourusername/json-db-sync.git
cd json-db-sync/sync-workspace
docker-compose up -d
```

This starts PostgreSQL and the sync server on port 8080.

### Option 2: Manual Setup

1. **Install dependencies:**
```bash
cargo build --workspace
```

2. **Set up PostgreSQL:**
```bash
createdb sync_db
export DATABASE_URL="postgres://user:password@localhost/sync_db"
```

3. **Run migrations:**
```bash
cd sync-server
sqlx migrate run
```

4. **Generate API credentials:**
```bash
cargo run --bin sync-server generate-credentials --name "My Application"
```

Save the generated API key and secret securely. The secret will not be shown again.

5. **Start the server:**
```bash
cargo run --bin sync-server serve
```

### Try the Interactive Client

```bash
# Using your generated API credentials
cargo run --package sync-client --example interactive_client -- \
    --api-key "rpa_your_api_key_here"

# Or specify a different database and user
cargo run --package sync-client --example interactive_client -- \
    --database bob \
    --api-key "rpa_your_api_key_here" \
    --user-id "user@example.com"
```

The interactive client provides a task management interface with:
- Create, edit, and complete tasks
- Priority levels and tags
- Rich task listing with status indicators
- Real-time sync across multiple clients
- Offline support with automatic sync when reconnected

## API Usage

### Rust Client Library

```rust
use sync_client::SyncEngine;
use serde_json::json;

// Connect to server with HMAC authentication
let engine = SyncEngine::new(
    "sqlite:client.db?mode=rwc",
    "ws://localhost:8080/ws",
    "rpa_your_api_key_here",
    "user@example.com"
).await?;

// Create a document
let doc = engine.create_document(
    json!({
        "title": "My Document",
        "description": "Task description",
        "status": "pending",
        "priority": "medium",
        "tags": ["work", "important"]
    })
).await?;

// Update document
engine.update_document(doc.id, json!({
    "status": "completed"
})).await?;
```

#### Rust Event Callbacks

```rust
use sync_client::events::{EventDispatcher, EventType};

// Get event dispatcher from engine
let events = engine.event_dispatcher();

// Register callback for document events
events.register_callback(
    |event_type, document_id, title, content, error, numeric_data, boolean_data, context| {
        match event_type {
            EventType::DocumentCreated => {
                println!("New document: {}", title.unwrap_or("untitled"));
            },
            EventType::DocumentUpdated => {
                println!("Updated: {}", title.unwrap_or("untitled"));
            },
            EventType::SyncCompleted => {
                println!("Synced {} documents", numeric_data);
            },
            _ => {}
        }
    },
    std::ptr::null_mut(), // context
    None // filter (None = all events)
)?;

// In your main loop
loop {
    let processed = events.process_events()?;
    if processed > 0 {
        println!("Processed {} events", processed);
    }
    
    tokio::time::sleep(tokio::time::Duration::from_millis(16)).await;
}
```

### WebSocket API

Connect to `ws://localhost:8080/ws` and authenticate with HMAC signature:

```json
{
  "type": "authenticate",
  "email": "user@example.com",
  "client_id": "550e8400-e29b-41d4-a716-446655440000",
  "api_key": "rpa_your_api_key_here",
  "signature": "calculated_hmac_signature",
  "timestamp": 1736525432
}
```

The HMAC signature is calculated as: `HMAC-SHA256(secret, "timestamp.email.api_key.body")`

Create documents:
```json
{
  "type": "create_document",
  "document": {
    "id": "550e8400-e29b-41d4-a716-446655440001",
    "title": "My Document",
    "content": {"text": "Hello, World!"}
  }
}
```

Update with JSON patches:
```json
{
  "type": "update_document",
  "patch": {
    "document_id": "550e8400-e29b-41d4-a716-446655440001",
    "patch": [
      {"op": "replace", "path": "/text", "value": "Updated text"}
    ]
  }
}
```

### C/C++ Integration

The sync client provides a C API that can be used from C, C++, and other languages. Build the distribution SDK:

```bash
# Build the complete SDK with headers and libraries
./scripts/build_dist.sh

# This creates:
# - dist/include/sync_client.h    (C header)
# - dist/include/sync_client.hpp  (C++ wrapper)
# - dist/lib/                     (compiled libraries)
# - dist/examples/                (working examples)
```

#### Event Callback System

The sync client includes a comprehensive event callback system for real-time notifications about document changes, sync operations, and connection status.

**Key Features:**
- Thread-safe design with no locks needed in user code
- Support for Rust, C, and C++ applications
- Event filtering and context passing
- Offline and online event notifications

**Quick C Example:**
```c
#include "sync_client_events.h"

void my_callback(const SyncEventData* event, void* context) {
    switch (event->event_type) {
        case SYNC_EVENT_DOCUMENT_CREATED:
            printf("Document created: %s\n", event->title);
            break;
        case SYNC_EVENT_SYNC_COMPLETED:
            printf("Sync completed: %llu documents\n", event->numeric_data);
            break;
    }
}

int main() {
    struct CSyncEngine* engine = sync_engine_create(
        "sqlite:client.db",
        "ws://localhost:8080/ws",
        "user@example.com",
        "rpa_your_api_key_here",
        "rps_your_secret_here");
    
    // Register callback for all events
    sync_engine_register_event_callback(engine, my_callback, NULL, -1);
    
    // Main loop - process events regularly
    while (running) {
        uint32_t processed_count;
        sync_engine_process_events(engine, &processed_count);
        // ... do other work
        usleep(16000); // ~60 FPS
    }
    
    sync_engine_destroy(engine);
    return 0;
}
```

**C++ Example with Lambdas:**
```cpp
#include "sync_client_events.h"

int main() {
    // Simple RAII wrapper
    SyncEngine engine("sqlite:client.db", "ws://localhost:8080/ws",
                     "user@example.com", "rpa_your_api_key_here",
                     "rps_your_secret_here");
    
    // Lambda callback with capture
    int event_count = 0;
    auto callback = [&event_count](const SyncEventData* event, void* context) {
        event_count++;
        std::cout << "Event #" << event_count << ": " 
                 << get_event_name(event->event_type) << std::endl;
    };
    
    // Register callback
    engine.register_callback(callback);
    
    // Main loop
    while (running) {
        engine.process_events();
        std::this_thread::sleep_for(std::chrono::milliseconds(16));
    }
    
    return 0;
}
```

For complete documentation and advanced examples, see [EVENT_CALLBACKS.md](EVENT_CALLBACKS.md).

#### Framework Integration

The event callback system integrates with desktop frameworks via timers:

- **JUCE**: Use `juce::Timer` to call `process_events()` every 16ms
- **Qt**: Use `QTimer` with Qt's signal/slot system  
- **GTK**: Use `g_timeout_add()` for periodic processing
- All callbacks execute on the framework's main thread

See [FRAMEWORK_INTEGRATION.md](FRAMEWORK_INTEGRATION.md) for complete examples.

**C API Example:**

```c
#include "sync_client.h"
#include <stdio.h>
#include <stdlib.h>

int main() {
    // Create sync engine with HMAC authentication
    struct CSyncEngine* engine = sync_engine_create(
        "sqlite:client.db?mode=rwc",
        "ws://localhost:8080/ws",
        "user@example.com",
        "rpa_your_api_key_here",
        "rps_your_secret_here"
    );
    
    if (!engine) {
        printf("Failed to create sync engine\n");
        return 1;
    }
    
    // Get version
    char* version = sync_get_version();
    if (version) {
        printf("Sync client version: %s\n", version);
        sync_string_free(version);
    }
    
    // Create a document
    char doc_id[37] = {0}; // UUID string + null terminator
    enum CSyncResult result = sync_engine_create_document(
        engine,
        "{\"title\":\"My Document\",\"content\":\"Hello World\",\"type\":\"note\",\"priority\":\"medium\"}",
        doc_id
    );
    
    if (result == Success) {
        printf("Created document: %s\n", doc_id);
        
        // Update the document
        result = sync_engine_update_document(
            engine,
            doc_id,
            "{\"content\":\"Hello Updated World\",\"type\":\"note\",\"priority\":\"high\"}"
        );
        
        if (result == Success) {
            printf("Updated document successfully\n");
        }
    }
    
    // Clean up
    sync_engine_destroy(engine);
    return 0;
}
```

**C++ Integration:**

```cpp
#include "replicant.hpp"
#include <iostream>

int main()
{
    try
    {
        // Create client with HMAC authentication
        replicant::Client client(
            "sqlite:client.db?mode=rwc",
            "ws://localhost:8080/ws",
            "user@example.com",
            "rpa_your_api_key_here",
            "rps_your_secret_here"
        );

        std::cout << "Replicant version: " << replicant::Client::get_version() << std::endl;

        // Create a document
        auto doc_id = client.create_document(
            R"({"title":"My Document","content":"Hello World","type":"note","priority":"medium"})"
        );

        std::cout << "Created document: " << doc_id << std::endl;

        // Update the document
        client.update_document(
            doc_id,
            R"({"content":"Hello Updated World","type":"note","priority":"high"})"
        );

        std::cout << "Updated document successfully" << std::endl;
    }
    catch (const replicant::SyncException& e)
    {
        std::cerr << "Replicant error: " << e.what() << std::endl;
        return 1;
    }

    return 0;
}
```

**Building and Linking:**

Using the distribution SDK (recommended):

```cmake
# CMakeLists.txt
cmake_minimum_required(VERSION 3.15)
project(MyProject)

set(CMAKE_CXX_STANDARD 11)

# Find the sync client SDK
find_package(sync_client REQUIRED PATHS /path/to/sync-workspace/dist)

# Your C++ application
add_executable(my_app main.cpp)
target_link_libraries(my_app sync_client)
```

Or compile directly:

```bash
# Build the distribution first
./scripts/build_dist.sh

# Compile your C/C++ code (example for macOS/Linux)
gcc -I./dist/include your_code.c -L./dist/lib -lsync_client -framework Security -o your_program

# For C++
g++ -std=c++11 -I./dist/include your_code.cpp -L./dist/lib -lsync_client -framework Security -o your_program
```

## Bidirectional Patch System

The system stores both forward and reverse patches for every document change:

### How It Works

**CREATE Event:**
- `forward_patch`: Contains the full document as initial state
- `reverse_patch`: `null` (creation cannot be undone)

**UPDATE Event:**  
- `forward_patch`: JSON patch to apply the change
- `reverse_patch`: JSON patch to undo the change

**DELETE Event:**
- `forward_patch`: `null` (deletion is implicit)  
- `reverse_patch`: Contains full document to restore if undeleted

### Benefits

- **Complete Audit Trail**: All changes logged with recovery capabilities
- **Efficient Storage**: JSON patches store only the differences between versions
- **Version History**: Forward and reverse patch support for future undo functionality  

## Conflict Resolution

The system handles concurrent updates with basic conflict detection:

1. **Multiple clients** can update the same document simultaneously
2. **Server processes updates** sequentially and detects conflicts using vector clocks
3. **Server-wins fallback** for conflict resolution (clients accept server state)
4. **Conflict detection** implemented with vector clock comparison

## Testing

### Unit Tests
```bash
cargo test --lib --bins
```

### Integration Tests
```bash
# Local setup with PostgreSQL (fast)
./test/run_integration_tests_local.sh

# Docker-based setup (consistent environment)
./test/run_integration_tests_docker.sh

# Manual setup
docker-compose -f docker-compose.test.yml up -d
export RUN_INTEGRATION_TESTS=1
export TEST_DATABASE_URL="postgres://postgres:postgres@localhost:5433/sync_test_db"
cargo test integration -- --test-threads=1
```

### Test Coverage
- **sync-core**: 7 tests (vector clocks, document revisions, JSON patches)
- **sync-client**: 1 test (basic functionality)  
- **sync-server**: 7 tests (authentication, WebSocket protocol, concurrent scenarios)

## Authentication

The system uses a two-level authentication model with HMAC-based request signing:

### Authentication Levels

1. **Application Level**: API credentials authenticate the application
   - API Key format: `rpa_*` (Replicant API)
   - Secret format: `rps_*` (Replicant Secret)
   - Credentials are generated using the CLI tool
   - One set of credentials per application/environment

2. **User Level**: Email identifies which user's data to access
   - Users are identified by email address
   - No passwords required (security handled at application level)
   - User records created automatically on first authentication

### Generating Credentials

Generate API credentials using the server CLI:

```bash
cargo run --bin sync-server generate-credentials --name "Production"
```

This outputs:
```
API Key:    rpa_a8d73487645ef2b9c3d4e5f6a7b8c9d0
Secret:     rps_1f2e3d4c5b6a798897a6b5c4d3e2f1a0
```

**Important**: Save the secret securely - it will not be shown again.

### HMAC Signature

All authenticated requests require an HMAC-SHA256 signature:

1. **Message format**: `timestamp.email.api_key.body`
2. **Timestamp validation**: Requests expire after 5 minutes
3. **Signature verification**: Prevents tampering and replay attacks

### Security Considerations

- **Transport Security**: Use WSS/HTTPS in production
- **Credential Storage**: Store API secrets securely (environment variables, secrets manager)
- **Rate Limiting**: Implement rate limiting to prevent brute force attempts
- **Audit Logging**: Track authentication attempts and failures

## Performance & Security

### Optimizations
- **Connection pooling** for PostgreSQL and SQLite
- **JSONB indexing** for fast document searches
- **Patch-based storage** for efficient change tracking

### Security Features
- HMAC-based authentication with timestamp validation
- API credentials stored in database (plaintext for MVP)
- Input validation for JSON patches
- Replay attack prevention via timestamp checks

## License

MIT

## Contributing

1. Fork the repository
2. Create a feature branch
3. Add tests for new functionality
4. Run `cargo test` and `./test/run_integration_tests_docker.sh`
5. Submit a pull request

See [TESTING.md](TESTING.md) for detailed testing guidelines and [EXAMPLES.md](EXAMPLES.md) for usage examples.

## Release gate

Every binary that links replicant-client carries `replicant-client-version=<version>`. Before a release, check that all of them link the same version:

    python3 scripts/check_replicant_versions.py <app bundle> <plugin bundles…>

It exits 1 when a binary has no marker, carries two versions, or the binaries disagree.
