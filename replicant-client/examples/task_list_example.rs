use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use replicant_client::events::SyncEvent;
use replicant_client::{Client, ClientDatabase};
use replicant_core::models::Document;
use serde_json::{json, Value};
use sqlx::Row;
use std::{
    error::Error,
    io::{self, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

// Application identifier for namespace generation
const APP_ID: &str = "com.example.sync-task-list";

fn debug_log(msg: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/task_list_debug.log")
        .unwrap();
    let timestamp = chrono::Utc::now().format("%H:%M:%S%.3f");
    writeln!(file, "[{}] {}", timestamp, msg).unwrap();
}

#[derive(Parser)]
#[command(name = "task-list")]
#[command(about = "Task list manager with real-time sync", long_about = None)]
struct Cli {
    /// Database file name (will auto-create in databases/ directory)
    #[arg(short, long, default_value = "tasks")]
    database: String,

    /// Auto-generate unique database name for concurrent testing
    #[arg(short, long)]
    auto: bool,

    /// User identifier (email/username) for shared identity across clients
    #[arg(short, long)]
    user: Option<String>,

    /// Server WebSocket URL (Phoenix server)
    #[arg(short, long, default_value = "ws://localhost:4000/socket/websocket")]
    server: String,

    /// API key for authentication (from: mix replicant.gen.credentials)
    #[arg(short = 'k', long, env = "REPLICANT_API_KEY")]
    api_key: String,

    /// API secret for authentication (from: mix replicant.gen.credentials)
    #[arg(long, env = "REPLICANT_API_SECRET")]
    api_secret: String,
}

#[derive(Clone)]
struct Task {
    id: Uuid,
    title: String,
    description: String,
    status: String,
    priority: String,
    tags: Vec<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    sync_revision: i64,
    sync_status: Option<String>,
}

struct ActivityEntry {
    timestamp: chrono::DateTime<chrono::Utc>,
    message: String,
    event_type: ActivityType,
}

#[derive(Clone, Copy)]
enum ActivityType {
    Created,
    Updated,
    Deleted,
    SyncStarted,
    SyncCompleted,
    Connected,
    Disconnected,
    Error,
}

struct AppState {
    tasks: Vec<Task>,
    selected_task: usize,
    activity_log: Vec<ActivityEntry>,
    sync_status: SyncStatus,
    last_sync: Option<Instant>,
    should_quit: bool,
    needs_refresh: bool,
    last_refresh: Option<Instant>,
    database_name: String,
    edit_mode: Option<EditMode>,
}

#[derive(Clone)]
struct EditMode {
    task_id: Uuid,
    field: EditField,
    input: String,
    cursor_pos: usize,
    priority: Priority,
}

#[derive(Clone, PartialEq)]
enum EditField {
    Title,
    Description,
    Priority,
}

impl EditField {
    fn next(&self) -> Self {
        match self {
            EditField::Title => EditField::Description,
            EditField::Description => EditField::Priority,
            EditField::Priority => EditField::Title,
        }
    }

    fn previous(&self) -> Self {
        match self {
            EditField::Title => EditField::Priority,
            EditField::Description => EditField::Title,
            EditField::Priority => EditField::Description,
        }
    }
}

#[derive(Clone, PartialEq)]
enum Priority {
    Low,
    Medium,
    High,
}

impl Priority {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "low" | "l" => Priority::Low,
            "high" | "h" => Priority::High,
            _ => Priority::Medium,
        }
    }

    fn to_string(&self) -> String {
        match self {
            Priority::Low => "low".to_string(),
            Priority::Medium => "medium".to_string(),
            Priority::High => "high".to_string(),
        }
    }

    fn to_display(&self) -> &'static str {
        match self {
            Priority::Low => "🟢 Low",
            Priority::Medium => "🟡 Medium",
            Priority::High => "🔴 High",
        }
    }

    fn next(&self) -> Self {
        match self {
            Priority::Low => Priority::Medium,
            Priority::Medium => Priority::High,
            Priority::High => Priority::Low,
        }
    }

    fn previous(&self) -> Self {
        match self {
            Priority::Low => Priority::High,
            Priority::Medium => Priority::Low,
            Priority::High => Priority::Medium,
        }
    }
}

#[derive(Clone)]
struct SyncStatus {
    connected: bool,
    pending_count: usize,
    conflict_count: usize,
    connection_state: String,
    last_attempt: Option<Instant>,
    next_retry: Option<Instant>,
}

impl AppState {
    fn new(database_name: String) -> Self {
        Self {
            tasks: Vec::new(),
            selected_task: 0,
            activity_log: Vec::new(),
            sync_status: SyncStatus {
                connected: false,
                pending_count: 0,
                conflict_count: 0,
                connection_state: "Starting...".to_string(),
                last_attempt: None,
                next_retry: None,
            },
            last_sync: None,
            should_quit: false,
            needs_refresh: true,
            last_refresh: None,
            database_name,
            edit_mode: None,
        }
    }

    fn add_activity(&mut self, message: String, event_type: ActivityType) {
        self.activity_log.push(ActivityEntry {
            timestamp: chrono::Utc::now(),
            message,
            event_type,
        });
        // Keep only last 20 entries
        if self.activity_log.len() > 20 {
            self.activity_log.remove(0);
        }
    }

    fn get_selected_task(&self) -> Option<&Task> {
        self.tasks.get(self.selected_task)
    }

    fn move_selection_up(&mut self) {
        if self.selected_task > 0 {
            self.selected_task -= 1;
        }
    }

    fn move_selection_down(&mut self) {
        if self.selected_task < self.tasks.len().saturating_sub(1) {
            self.selected_task += 1;
        }
    }

    fn start_edit(&mut self, field: EditField) {
        if let Some(task) = self.get_selected_task() {
            let initial_value = match field {
                EditField::Title => task.title.clone(),
                EditField::Description => task.description.clone(),
                EditField::Priority => task.priority.clone(),
            };

            self.edit_mode = Some(EditMode {
                task_id: task.id,
                field,
                input: initial_value.clone(),
                cursor_pos: initial_value.len(),
                priority: Priority::from_str(&task.priority),
            });
        }
    }

    fn handle_edit_input(&mut self, ch: char) {
        if let Some(ref mut edit) = self.edit_mode {
            edit.input.insert(edit.cursor_pos, ch);
            edit.cursor_pos += 1;
        }
    }

    fn handle_edit_backspace(&mut self) {
        if let Some(ref mut edit) = self.edit_mode {
            if edit.cursor_pos > 0 {
                edit.cursor_pos -= 1;
                edit.input.remove(edit.cursor_pos);
            }
        }
    }

    fn handle_edit_cursor_left(&mut self) {
        if let Some(ref mut edit) = self.edit_mode {
            if edit.cursor_pos > 0 {
                edit.cursor_pos -= 1;
            }
        }
    }

    fn handle_edit_cursor_right(&mut self) {
        if let Some(ref mut edit) = self.edit_mode {
            if edit.cursor_pos < edit.input.len() {
                edit.cursor_pos += 1;
            }
        }
    }

    fn cancel_edit(&mut self) {
        self.edit_mode = None;
    }

    fn is_editing(&self) -> bool {
        self.edit_mode.is_some()
    }

    fn cycle_edit_field_next(&mut self) {
        if let Some(edit) = &self.edit_mode {
            // Get task data first
            let task_data = if let Some(task) = self.get_selected_task() {
                if edit.task_id == task.id {
                    Some((
                        task.title.clone(),
                        task.description.clone(),
                        task.priority.clone(),
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            // Now modify edit mode
            if let (Some((title, description, priority)), Some(ref mut edit)) =
                (task_data, &mut self.edit_mode)
            {
                let new_field = edit.field.next();
                let initial_value = match new_field {
                    EditField::Title => title,
                    EditField::Description => description,
                    EditField::Priority => priority.clone(),
                };
                edit.field = new_field;
                edit.input = initial_value.clone();
                edit.cursor_pos = initial_value.len();
                edit.priority = Priority::from_str(&priority);
            }
        }
    }

    fn cycle_edit_field_previous(&mut self) {
        if let Some(edit) = &self.edit_mode {
            // Get task data first
            let task_data = if let Some(task) = self.get_selected_task() {
                if edit.task_id == task.id {
                    Some((
                        task.title.clone(),
                        task.description.clone(),
                        task.priority.clone(),
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            // Now modify edit mode
            if let (Some((title, description, priority)), Some(ref mut edit)) =
                (task_data, &mut self.edit_mode)
            {
                let new_field = edit.field.previous();
                let initial_value = match new_field {
                    EditField::Title => title,
                    EditField::Description => description,
                    EditField::Priority => priority.clone(),
                };
                edit.field = new_field;
                edit.input = initial_value.clone();
                edit.cursor_pos = initial_value.len();
                edit.priority = Priority::from_str(&priority);
            }
        }
    }

    fn cycle_priority_next(&mut self) {
        if let Some(ref mut edit) = self.edit_mode {
            if matches!(edit.field, EditField::Priority) {
                edit.priority = edit.priority.next();
                edit.input = edit.priority.to_string();
            }
        }
    }

    fn cycle_priority_previous(&mut self) {
        if let Some(ref mut edit) = self.edit_mode {
            if matches!(edit.field, EditField::Priority) {
                edit.priority = edit.priority.previous();
                edit.input = edit.priority.to_string();
            }
        }
    }
}

// Thread-safe wrapper for app state
type SharedState = Arc<Mutex<AppState>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Clear the debug log file and start logging
    std::fs::write("/tmp/task_list_debug.log", "").unwrap();
    debug_log("=== Task List Example Starting ===");

    // Initialize comprehensive logging to file for debugging
    tracing_subscriber::fmt()
        .with_env_filter("sync_client=info,replicant_core=info,task_list_example=info")
        .with_writer(
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open("/tmp/sync_debug.log")
                .unwrap(),
        )
        .init();

    let cli = Cli::parse();
    debug_log(&format!(
        "CLI args - database: {}, auto: {}, user: {:?}",
        cli.database, cli.auto, cli.user
    ));

    // Setup database
    std::fs::create_dir_all("databases")?;

    // Generate database name
    let db_name = if cli.auto {
        // Auto-generate unique name with timestamp and random suffix
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let random_suffix = Uuid::new_v4().to_string()[..8].to_string();
        format!("client_{}_{}", timestamp, random_suffix)
    } else {
        cli.database.clone()
    };

    let db_file = format!("databases/{}.sqlite3", db_name);
    let db_url = format!("sqlite:{}?mode=rwc", db_file);

    let db = Arc::new(ClientDatabase::new(&db_url).await?);
    db.run_migrations().await?;

    // Get or create user
    let user_id = match db.get_user_id().await {
        Ok(id) => id,
        Err(_) => {
            // Generate deterministic user ID based on user identifier or create random
            let id = if let Some(user_identifier) = &cli.user {
                // Use UUID v5 for deterministic ID generation
                // This creates a two-level namespace hierarchy:
                // 1. DNS namespace -> Application namespace (using APP_ID)
                // 2. Application namespace -> User ID (using user identifier)
                // This ensures our user IDs are unique to our application
                let app_namespace = Uuid::new_v5(&Uuid::NAMESPACE_DNS, APP_ID.as_bytes());
                Uuid::new_v5(&app_namespace, user_identifier.as_bytes())
            } else {
                Uuid::new_v4()
            };

            // Client ID should always be unique per client instance
            let client_id = Uuid::new_v4();
            setup_user(&db, id, client_id, &cli.server, &cli.api_key).await?;
            id
        }
    };

    // Create shared state
    let state = Arc::new(Mutex::new(AppState::new(db_name.clone())));

    // Load initial tasks
    {
        let mut app_state = state.lock().unwrap();
        app_state.add_activity(
            format!("Loading tasks for user: {}", user_id),
            ActivityType::SyncStarted,
        );
    }

    match load_tasks(&db, user_id, state.clone()).await {
        Ok(_) => {
            let task_count = state.lock().unwrap().tasks.len();
            let mut app_state = state.lock().unwrap();
            app_state.add_activity(
                format!("Loaded {} tasks", task_count),
                ActivityType::SyncCompleted,
            );
        }
        Err(e) => {
            let mut app_state = state.lock().unwrap();
            app_state.add_activity(
                format!("Failed to load initial tasks: {}", e),
                ActivityType::Error,
            );
            // Don't propagate the error - let the UI start anyway
        }
    }

    // Setup terminal first - we want to show the UI immediately
    debug_log("Enabling raw mode...");
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    debug_log("Setting up terminal...");
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    debug_log(&format!(
        "Terminal initialized, size: {:?}",
        terminal.size()?
    ));

    // Initialize UI with startup message
    {
        let mut app_state = state.lock().unwrap();
        app_state.add_activity(
            "Task Manager starting...".to_string(),
            ActivityType::SyncStarted,
        );
        if cli.auto {
            app_state.add_activity(
                format!("Created new database: {}", db_name),
                ActivityType::SyncStarted,
            );
        } else {
            app_state.add_activity(
                format!("Using database: {}", db_name),
                ActivityType::SyncStarted,
            );
        }
        if let Some(user_identifier) = &cli.user {
            app_state.add_activity(
                format!("Using shared identity: {}", user_identifier),
                ActivityType::SyncStarted,
            );
        }
        app_state.sync_status.connection_state = "Starting...".to_string();
    }

    // Create sync engine - automatic reconnection is now built-in
    let user_email = cli.user.clone().unwrap_or_else(|| "anonymous".to_string());

    let sync_engine = match Client::new(
        &db_url,
        &cli.server,
        &user_email,
        &cli.api_key,
        &cli.api_secret,
    )
    .await
    {
        Ok(engine) => {
            // Register Rust-native callback for event handling
            let state_for_callback = state.clone();
            if let Err(e) = engine
                .event_dispatcher()
                .register_rust_callback(move |event| {
                    let mut app_state = state_for_callback.lock().unwrap();
                    match event {
                        SyncEvent::DocumentCreated { title, .. } => {
                            app_state.add_activity(
                                format!("Task created: {}", title),
                                ActivityType::Created,
                            );
                            app_state.needs_refresh = true;
                        }
                        SyncEvent::DocumentUpdated { title, .. } => {
                            app_state.add_activity(
                                format!("Task updated: {}", title),
                                ActivityType::Updated,
                            );
                            app_state.needs_refresh = true;
                        }
                        SyncEvent::DocumentDeleted { id } => {
                            app_state.add_activity(
                                format!("Task deleted: {}...", &id[..8.min(id.len())]),
                                ActivityType::Deleted,
                            );
                            app_state.needs_refresh = true;
                        }
                        SyncEvent::SyncStarted => {
                            app_state.add_activity(
                                "Sync started".to_string(),
                                ActivityType::SyncStarted,
                            );
                        }
                        SyncEvent::SyncCompleted { document_count } => {
                            app_state.add_activity(
                                format!("Sync completed ({} docs)", document_count),
                                ActivityType::SyncCompleted,
                            );
                            app_state.last_sync = Some(Instant::now());
                            app_state.needs_refresh = true;
                        }
                        SyncEvent::SyncError { message } => {
                            app_state.add_activity(
                                format!("Sync error: {}", message),
                                ActivityType::Error,
                            );
                        }
                        SyncEvent::ConnectionLost { server_url } => {
                            app_state.add_activity(
                                format!("Lost connection to {}", server_url),
                                ActivityType::Disconnected,
                            );
                            app_state.sync_status.connected = false;
                            app_state.sync_status.connection_state = "Disconnected".to_string();
                            app_state.needs_refresh = true;
                        }
                        SyncEvent::ConnectionAttempted { server_url } => {
                            app_state.add_activity(
                                format!("Connecting to {}", server_url),
                                ActivityType::SyncStarted,
                            );
                            app_state.sync_status.connection_state = "Connecting...".to_string();
                        }
                        SyncEvent::ConnectionSucceeded { server_url } => {
                            app_state.add_activity(
                                format!("Connected to {}", server_url),
                                ActivityType::Connected,
                            );
                            app_state.sync_status.connected = true;
                            app_state.sync_status.connection_state = "Connected".to_string();
                        }
                        SyncEvent::ConflictDetected { document_id, .. } => {
                            app_state.add_activity(
                                format!(
                                    "Conflict detected: {}...",
                                    &document_id[..8.min(document_id.len())]
                                ),
                                ActivityType::Error,
                            );
                        }
                    }
                })
            {
                debug_log(&format!("Failed to register callback: {:?}", e));
            }

            {
                let mut app_state = state.lock().unwrap();
                app_state.add_activity(
                    "Sync engine initialized".to_string(),
                    ActivityType::SyncStarted,
                );
                app_state.sync_status.connection_state = "Auto-connecting...".to_string();
            }
            Arc::new(Mutex::new(Some(Arc::new(engine))))
        }
        Err(e) => {
            {
                let mut app_state = state.lock().unwrap();
                app_state.add_activity(
                    format!("Engine initialization failed: {}", e),
                    ActivityType::Error,
                );
                app_state.sync_status.connection_state = "Initialization failed".to_string();
            }
            Arc::new(Mutex::new(None))
        }
    };

    // Run app
    debug_log("Starting run_app...");
    let res = run_app(
        &mut terminal,
        state.clone(),
        db.clone(),
        sync_engine,
        user_id,
    )
    .await;
    debug_log(&format!("run_app finished with result: {:?}", res.is_ok()));

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{err:?}");
    }

    println!("\n📁 Database file: {}", db_file);
    println!(
        "   Full path: {}",
        std::path::Path::new(&db_file)
            .canonicalize()
            .unwrap_or_default()
            .display()
    );

    Ok(())
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    state: SharedState,
    db: Arc<ClientDatabase>,
    sync_engine: Arc<Mutex<Option<Arc<Client>>>>,
    user_id: Uuid,
) -> io::Result<()> {
    debug_log(&format!("run_app started for user: {}", user_id));

    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(250);

    // Track the last known connection state
    let mut last_connection_state: Option<bool> = None;

    debug_log("Starting main loop...");
    loop {
        // Process sync events FIRST
        if let Some(engine) = sync_engine.lock().unwrap().as_ref() {
            let dispatcher = engine.event_dispatcher();
            match dispatcher.process_events() {
                Ok(count) if count > 0 => {
                    // Events were processed - this might set needs_refresh
                }
                Err(e) => eprintln!("Error processing events: {:?}", e),
                _ => {}
            }

            // Check connection state and emit event if it changed
            let current_connected = engine.is_connected();
            match last_connection_state {
                Some(last_connected) if last_connected != current_connected => {
                    // Connection state changed
                    let mut app_state = state.lock().unwrap();
                    if current_connected {
                        app_state.add_activity(
                            "Connected to server".to_string(),
                            ActivityType::Connected,
                        );
                        app_state.sync_status.connected = true;
                        app_state.sync_status.connection_state = "Connected".to_string();
                    } else {
                        app_state.add_activity(
                            "Disconnected from server".to_string(),
                            ActivityType::Disconnected,
                        );
                        app_state.sync_status.connected = false;
                        app_state.sync_status.connection_state = "Disconnected".to_string();
                    }
                }
                None => {
                    // First time checking - set initial state
                    let mut app_state = state.lock().unwrap();
                    app_state.sync_status.connected = current_connected;
                    app_state.sync_status.connection_state = if current_connected {
                        "Connected".to_string()
                    } else {
                        "Disconnected".to_string()
                    };
                }
                _ => {} // No change
            }
            last_connection_state = Some(current_connected);
        }

        // Check if we should refresh tasks
        {
            let should_refresh = {
                let mut app_state = state.lock().unwrap();
                if app_state.needs_refresh {
                    app_state.needs_refresh = false;
                    true
                } else {
                    false
                }
            };

            if should_refresh {
                // Load tasks with error handling
                if let Err(e) = load_tasks(&db, user_id, state.clone()).await {
                    let mut app_state = state.lock().unwrap();
                    app_state
                        .add_activity(format!("Failed to load tasks: {}", e), ActivityType::Error);
                } else {
                    // Mark refresh time for visual feedback only on success
                    let mut app_state = state.lock().unwrap();
                    app_state.last_refresh = Some(Instant::now());
                }

                update_sync_status(&db, state.clone()).await;
            }
        }

        // Draw UI once per loop iteration
        terminal.draw(|f| ui(f, &state))?;

        // Handle input with reasonable timeout
        let timeout = Duration::from_millis(100);
        if crossterm::event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key_event(
                        key.code,
                        state.clone(),
                        db.clone(),
                        sync_engine.clone(),
                        user_id,
                    )
                    .await;
                }
            }
        }

        // Check if we should quit
        if state.lock().unwrap().should_quit {
            return Ok(());
        }

        // Update tick for time tracking
        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }
    }
}

fn ui(f: &mut Frame, state: &SharedState) {
    let app_state = state.lock().unwrap();

    // Main layout - title bar and content
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Title bar
            Constraint::Min(0),    // Content
        ])
        .split(f.size());

    // Title bar with refresh indicator
    let title = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));

    let refresh_indicator = if let Some(last_refresh) = app_state.last_refresh {
        let elapsed = last_refresh.elapsed();
        if elapsed.as_secs() < 2 {
            " ●" // Show indicator for 2 seconds after refresh
        } else {
            ""
        }
    } else {
        ""
    };

    let title_text = format!(
        "Task Manager [{}]{}",
        app_state.database_name, refresh_indicator
    );

    let title_paragraph = Paragraph::new(title_text)
        .block(title)
        .alignment(Alignment::Center);
    f.render_widget(title_paragraph, main_chunks[0]);

    // Content area - 2x2 grid
    let content_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(main_chunks[1]);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
        .split(content_chunks[0]);

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
        .split(content_chunks[1]);

    // Top-left: Task list
    render_task_list(f, left_chunks[0], &app_state);

    // Top-right: Task details
    render_task_details(f, right_chunks[0], &app_state);

    // Bottom-left: Sync status
    render_sync_status(f, left_chunks[1], &app_state);

    // Bottom-right: Activity log
    render_activity_log(f, right_chunks[1], &app_state);
}

fn render_task_list(f: &mut Frame, area: Rect, app_state: &AppState) {
    let todo_count = app_state
        .tasks
        .iter()
        .filter(|t| t.status != "completed")
        .count();
    let block = Block::default().borders(Borders::ALL).title(format!(
        "Tasks ({} total, {} todo)",
        app_state.tasks.len(),
        todo_count
    ));

    let items: Vec<ListItem> = app_state
        .tasks
        .iter()
        .enumerate()
        .map(|(idx, task)| {
            let status_icon = match task.status.as_str() {
                "completed" => "✅",
                "in_progress" => "🔄",
                "pending" => "⏳",
                _ => "❓",
            };

            let priority_icon = match task.priority.as_str() {
                "high" => "🔴",
                "medium" => "🟡",
                "low" => "🟢",
                _ => "⚪",
            };

            let sync_icon = match task.sync_status.as_deref() {
                Some("pending") => " 📤",
                Some("conflict") => " ⚠️",
                _ => "",
            };

            let content = format!(
                "{} {} {}{}",
                status_icon, priority_icon, task.title, sync_icon
            );
            let style = if idx == app_state.selected_task {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(content).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut list_state = ListState::default();
    list_state.select(Some(app_state.selected_task));

    f.render_stateful_widget(list, area, &mut list_state);

    // Render help text at bottom
    let help_text = if app_state.is_editing() {
        let current_field = app_state
            .edit_mode
            .as_ref()
            .map(|e| match e.field {
                EditField::Title => "Title",
                EditField::Description => "Description",
                EditField::Priority => "Priority",
            })
            .unwrap_or("Unknown");

        vec![
            Line::from(vec![
                Span::styled(
                    "EDIT MODE",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" - Editing {}", current_field)),
            ]),
            Line::from(vec![Span::raw(
                "[Enter: save] [Esc: cancel] [Tab/↓: next field] [↑: prev field]",
            )]),
            Line::from(vec![if current_field == "Priority" {
                Span::raw("[←/→: change priority] [Backspace: delete]")
            } else {
                Span::raw("[←/→: move cursor] [Backspace: delete]")
            }]),
        ]
    } else {
        vec![
            Line::from(vec![Span::raw("[j/k: navigate] [space: toggle]")]),
            Line::from(vec![Span::raw(
                "[e: edit title] [E: edit desc] [p: priority]",
            )]),
            Line::from(vec![Span::raw("[n: new] [d: delete] [q: quit]")]),
        ]
    };

    let help_area = Rect {
        x: area.x + 1,
        y: area.y + area.height - 4,
        width: area.width - 2,
        height: 3,
    };

    let help = Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(help, help_area);
}

fn render_text_with_cursor(text: &str, cursor_pos: usize) -> Line {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();

    // Add text before cursor
    if cursor_pos > 0 {
        let before: String = chars[..cursor_pos.min(chars.len())].iter().collect();
        spans.push(Span::styled(before, Style::default()));
    }

    // Add cursor character (invert the character at cursor position)
    if cursor_pos < chars.len() {
        let cursor_char = chars[cursor_pos].to_string();
        spans.push(Span::styled(
            cursor_char,
            Style::default().add_modifier(Modifier::REVERSED),
        ));

        // Add text after cursor
        if cursor_pos + 1 < chars.len() {
            let after: String = chars[cursor_pos + 1..].iter().collect();
            spans.push(Span::styled(after, Style::default()));
        }
    } else {
        // Cursor at end - show space as cursor (always visible even when text is empty)
        spans.push(Span::styled(
            " ",
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }

    Line::from(spans)
}

fn render_task_details(f: &mut Frame, area: Rect, app_state: &AppState) {
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .title("Task Details")
        .border_style(Style::default().fg(Color::White));

    // Create inner area within the outer block
    let inner_area = outer_block.inner(area);
    f.render_widget(outer_block, area);

    if let Some(task) = app_state.get_selected_task() {
        let status_display = match task.status.as_str() {
            "completed" => "✅ Completed",
            "in_progress" => "🔄 In Progress",
            "todo" => "📋 Todo",
            _ => &task.status,
        };

        let priority_display = match task.priority.as_str() {
            "high" => "🔴 High",
            "medium" => "🟡 Medium",
            "low" => "🟢 Low",
            _ => &task.priority,
        };

        // Create layout for editing mode vs normal mode
        if let Some(ref edit) = app_state.edit_mode {
            if edit.task_id == task.id {
                // Edit mode - create separate blocks for each field
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3), // Title block
                        Constraint::Length(3), // Priority block
                        Constraint::Length(4), // Description block
                        Constraint::Min(0),    // Status and metadata
                    ])
                    .split(inner_area);

                // Title block
                let title_block = if matches!(edit.field, EditField::Title) {
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Title")
                        .border_style(Style::default().fg(Color::Yellow))
                        .title_style(
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                } else {
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Title")
                        .border_style(Style::default().fg(Color::Cyan))
                };

                let title_content = if matches!(edit.field, EditField::Title) {
                    render_text_with_cursor(&edit.input, edit.cursor_pos)
                } else {
                    Line::from(vec![Span::styled(
                        &task.title,
                        Style::default().fg(Color::DarkGray),
                    )])
                };

                let title_paragraph = Paragraph::new(vec![title_content]).block(title_block);
                f.render_widget(title_paragraph, chunks[0]);

                // Priority block
                let priority_block = if matches!(edit.field, EditField::Priority) {
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Priority")
                        .border_style(Style::default().fg(Color::Yellow))
                        .title_style(
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                } else {
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Priority")
                        .border_style(Style::default().fg(Color::Cyan))
                };

                let priority_content = if matches!(edit.field, EditField::Priority) {
                    Line::from(vec![
                        Span::styled(
                            edit.priority.to_display(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" [←/→ to change]", Style::default().fg(Color::DarkGray)),
                    ])
                } else {
                    Line::from(vec![Span::styled(
                        priority_display,
                        Style::default().fg(Color::DarkGray),
                    )])
                };

                let priority_paragraph =
                    Paragraph::new(vec![priority_content]).block(priority_block);
                f.render_widget(priority_paragraph, chunks[1]);

                // Description block
                let description_block = if matches!(edit.field, EditField::Description) {
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Description")
                        .border_style(Style::default().fg(Color::Yellow))
                        .title_style(
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                } else {
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Description")
                        .border_style(Style::default().fg(Color::Cyan))
                };

                let description_content = if matches!(edit.field, EditField::Description) {
                    vec![render_text_with_cursor(&edit.input, edit.cursor_pos)]
                } else {
                    if !task.description.is_empty() {
                        vec![Line::from(vec![Span::styled(
                            &task.description,
                            Style::default().fg(Color::DarkGray),
                        )])]
                    } else {
                        vec![Line::from(vec![Span::styled(
                            "(empty)",
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        )])]
                    }
                };

                let description_paragraph = Paragraph::new(description_content)
                    .block(description_block)
                    .wrap(Wrap { trim: true });
                f.render_widget(description_paragraph, chunks[2]);

                // Status and metadata in remaining space
                let mut metadata_lines = vec![];
                metadata_lines.push(Line::from(vec![
                    Span::styled("Status: ", Style::default().fg(Color::Gray)),
                    Span::raw(status_display),
                ]));

                if !task.tags.is_empty() {
                    let tags_str = task
                        .tags
                        .iter()
                        .map(|t| format!("#{}", t))
                        .collect::<Vec<_>>()
                        .join(" ");
                    metadata_lines.push(Line::from(vec![
                        Span::styled("Tags: ", Style::default().fg(Color::Gray)),
                        Span::styled(tags_str, Style::default().fg(Color::Cyan)),
                    ]));
                }

                metadata_lines.push(Line::from(""));
                metadata_lines.push(Line::from(vec![
                    Span::styled("Created: ", Style::default().fg(Color::Gray)),
                    Span::raw(task.created_at.format("%Y-%m-%d %H:%M").to_string()),
                ]));
                metadata_lines.push(Line::from(vec![
                    Span::styled("Updated: ", Style::default().fg(Color::Gray)),
                    Span::raw(task.updated_at.format("%Y-%m-%d %H:%M").to_string()),
                ]));
                metadata_lines.push(Line::from(vec![
                    Span::styled("Version: ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        task.sync_revision.to_string(),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));

                let metadata_paragraph = Paragraph::new(metadata_lines).wrap(Wrap { trim: true });
                f.render_widget(metadata_paragraph, chunks[3]);

                return;
            }
        }

        // Normal mode - use same block layout for consistency
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Title block
                Constraint::Length(3), // Priority block
                Constraint::Length(4), // Description block
                Constraint::Min(0),    // Status and metadata
            ])
            .split(inner_area);

        // Title block (normal mode)
        let title_block = Block::default()
            .borders(Borders::ALL)
            .title("Title")
            .border_style(Style::default().fg(Color::Cyan));

        let title_content = Line::from(vec![Span::styled(
            &task.title,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]);

        let title_paragraph = Paragraph::new(vec![title_content]).block(title_block);
        f.render_widget(title_paragraph, chunks[0]);

        // Priority block (normal mode)
        let priority_block = Block::default()
            .borders(Borders::ALL)
            .title("Priority")
            .border_style(Style::default().fg(Color::Cyan));

        let priority_content = Line::from(vec![Span::raw(priority_display)]);

        let priority_paragraph = Paragraph::new(vec![priority_content]).block(priority_block);
        f.render_widget(priority_paragraph, chunks[1]);

        // Description block (normal mode)
        let description_block = Block::default()
            .borders(Borders::ALL)
            .title("Description")
            .border_style(Style::default().fg(Color::Cyan));

        let description_content = if !task.description.is_empty() {
            vec![Line::from(task.description.clone())]
        } else {
            vec![Line::from(vec![Span::styled(
                "(empty)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )])]
        };

        let description_paragraph = Paragraph::new(description_content)
            .block(description_block)
            .wrap(Wrap { trim: true });
        f.render_widget(description_paragraph, chunks[2]);

        // Status and metadata in remaining space
        let mut metadata_lines = vec![];
        metadata_lines.push(Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::Gray)),
            Span::raw(status_display),
        ]));

        if !task.tags.is_empty() {
            let tags_str = task
                .tags
                .iter()
                .map(|t| format!("#{}", t))
                .collect::<Vec<_>>()
                .join(" ");
            metadata_lines.push(Line::from(vec![
                Span::styled("Tags: ", Style::default().fg(Color::Gray)),
                Span::styled(tags_str, Style::default().fg(Color::Cyan)),
            ]));
        }

        metadata_lines.push(Line::from(""));
        metadata_lines.push(Line::from(vec![
            Span::styled("Created: ", Style::default().fg(Color::Gray)),
            Span::raw(task.created_at.format("%Y-%m-%d %H:%M").to_string()),
        ]));
        metadata_lines.push(Line::from(vec![
            Span::styled("Updated: ", Style::default().fg(Color::Gray)),
            Span::raw(task.updated_at.format("%Y-%m-%d %H:%M").to_string()),
        ]));
        metadata_lines.push(Line::from(vec![
            Span::styled("Version: ", Style::default().fg(Color::Gray)),
            Span::styled(
                task.sync_revision.to_string(),
                Style::default().fg(Color::Yellow),
            ),
        ]));

        let metadata_paragraph = Paragraph::new(metadata_lines).wrap(Wrap { trim: true });
        f.render_widget(metadata_paragraph, chunks[3]);
    } else {
        let paragraph =
            Paragraph::new("No task selected").style(Style::default().fg(Color::DarkGray));
        f.render_widget(paragraph, inner_area);
    }
}

fn render_sync_status(f: &mut Frame, area: Rect, app_state: &AppState) {
    let block = Block::default().borders(Borders::ALL).title("Sync Status");

    let connection_status = {
        let mut status_text = app_state.sync_status.connection_state.clone();

        // Show live countdown if we're waiting to retry
        if let Some(next_retry) = app_state.sync_status.next_retry {
            if !app_state.sync_status.connected {
                let now = Instant::now();
                if next_retry > now {
                    let seconds_left = (next_retry - now).as_secs();
                    if seconds_left > 0 {
                        status_text = format!("Offline (retry in {}s)", seconds_left);
                    }
                }
            }
        }

        Line::from(vec![
            Span::raw(if app_state.sync_status.connected {
                "🔗 "
            } else {
                "📡 "
            }),
            Span::styled(
                status_text,
                Style::default().fg(if app_state.sync_status.connected {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
        ])
    };

    let sync_info =
        if app_state.sync_status.pending_count > 0 || app_state.sync_status.conflict_count > 0 {
            if app_state.sync_status.conflict_count > 0 {
                Line::from(vec![
                    Span::raw("⚠️  "),
                    Span::styled(
                        format!("{} conflicts", app_state.sync_status.conflict_count),
                        Style::default().fg(Color::Red),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::raw("📤 "),
                    Span::styled(
                        format!("{} pending sync", app_state.sync_status.pending_count),
                        Style::default().fg(Color::Yellow),
                    ),
                ])
            }
        } else {
            Line::from(vec![Span::raw("✅ "), Span::raw("All changes synced")])
        };

    let last_sync_line = if let Some(last_sync) = app_state.last_sync {
        let elapsed = last_sync.elapsed();
        let time_str = if elapsed.as_secs() < 60 {
            format!("{} seconds ago", elapsed.as_secs())
        } else if elapsed.as_secs() < 3600 {
            format!("{} minutes ago", elapsed.as_secs() / 60)
        } else {
            format!("{} hours ago", elapsed.as_secs() / 3600)
        };
        Line::from(vec![Span::raw("📊 Last sync: "), Span::raw(time_str)])
    } else {
        Line::from(vec![
            Span::raw("📊 Last sync: "),
            Span::styled("Never", Style::default().fg(Color::DarkGray)),
        ])
    };

    let mut lines = vec![connection_status, sync_info, last_sync_line];

    // Add database filename info
    lines.push(Line::from(vec![
        Span::raw("💾 Database: "),
        Span::styled(&app_state.database_name, Style::default().fg(Color::Cyan)),
    ]));

    // Add retry info if we're offline and have attempted connections
    if !app_state.sync_status.connected && app_state.sync_status.last_attempt.is_some() {
        lines.push(Line::from(vec![Span::styled(
            "Auto-retrying in background",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "[n: new task] [q: quit]",
        Style::default().fg(Color::DarkGray),
    )]));

    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    f.render_widget(paragraph, area);
}

fn render_activity_log(f: &mut Frame, area: Rect, app_state: &AppState) {
    let block = Block::default().borders(Borders::ALL).title("Activity Log");

    let items: Vec<Line> = app_state
        .activity_log
        .iter()
        .rev()
        .map(|entry| {
            let icon = match entry.event_type {
                ActivityType::Created => "➕",
                ActivityType::Updated => "📝",
                ActivityType::Deleted => "🗑️",
                ActivityType::SyncStarted => "🔄",
                ActivityType::SyncCompleted => "✅",
                ActivityType::Connected => "🔗",
                ActivityType::Disconnected => "❌",
                ActivityType::Error => "🚨",
            };

            let time_str = entry.timestamp.format("%H:%M").to_string();

            Line::from(vec![
                Span::styled(time_str, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::raw(icon),
                Span::raw(" "),
                Span::raw(&entry.message),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(items).block(block).wrap(Wrap { trim: true });
    f.render_widget(paragraph, area);
}

async fn handle_key_event(
    key: KeyCode,
    state: SharedState,
    db: Arc<ClientDatabase>,
    sync_engine: Arc<Mutex<Option<Arc<Client>>>>,
    user_id: Uuid,
) {
    // Check if we're in edit mode
    let is_editing = state.lock().unwrap().is_editing();

    if is_editing {
        // Handle edit mode keys
        match key {
            KeyCode::Enter => {
                // Save the edit
                let edit_data = state.lock().unwrap().edit_mode.clone();
                if let Some(edit) = edit_data {
                    save_task_edit(&db, &sync_engine, edit, state.clone()).await;
                }
                state.lock().unwrap().cancel_edit();
            }
            KeyCode::Esc => {
                // Cancel edit
                state.lock().unwrap().cancel_edit();
            }
            KeyCode::Backspace => {
                state.lock().unwrap().handle_edit_backspace();
            }
            KeyCode::Left => {
                // Check if editing priority - if so, cycle priority, otherwise move cursor
                let is_editing_priority = state
                    .lock()
                    .unwrap()
                    .edit_mode
                    .as_ref()
                    .map(|e| matches!(e.field, EditField::Priority))
                    .unwrap_or(false);

                if is_editing_priority {
                    state.lock().unwrap().cycle_priority_previous();
                } else {
                    state.lock().unwrap().handle_edit_cursor_left();
                }
            }
            KeyCode::Right => {
                // Check if editing priority - if so, cycle priority, otherwise move cursor
                let is_editing_priority = state
                    .lock()
                    .unwrap()
                    .edit_mode
                    .as_ref()
                    .map(|e| matches!(e.field, EditField::Priority))
                    .unwrap_or(false);

                if is_editing_priority {
                    state.lock().unwrap().cycle_priority_next();
                } else {
                    state.lock().unwrap().handle_edit_cursor_right();
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                // Cycle to previous field
                state.lock().unwrap().cycle_edit_field_previous();
            }
            KeyCode::BackTab | KeyCode::Up => {
                // Cycle to next field
                state.lock().unwrap().cycle_edit_field_next();
            }
            KeyCode::Char(ch) => {
                // Only allow text input for title and description, not priority
                let is_editing_priority = state
                    .lock()
                    .unwrap()
                    .edit_mode
                    .as_ref()
                    .map(|e| matches!(e.field, EditField::Priority))
                    .unwrap_or(false);

                if !is_editing_priority {
                    state.lock().unwrap().handle_edit_input(ch);
                }
            }
            _ => {}
        }
    } else {
        // Handle normal mode keys
        match key {
            KeyCode::Char('q') => {
                state.lock().unwrap().should_quit = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                state.lock().unwrap().move_selection_down();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.lock().unwrap().move_selection_up();
            }
            KeyCode::Char(' ') => {
                // Toggle task completion
                let task_id = {
                    let app_state = state.lock().unwrap();
                    app_state.get_selected_task().map(|t| t.id)
                };

                if let Some(id) = task_id {
                    toggle_task_completion(&db, &sync_engine, id, state.clone()).await;
                }
            }
            KeyCode::Char('n') => {
                // Create new task
                create_sample_task(&db, &sync_engine, user_id, state.clone()).await;
            }
            KeyCode::Char('d') => {
                // Delete selected task
                let task_id = {
                    let app_state = state.lock().unwrap();
                    app_state.get_selected_task().map(|t| t.id)
                };

                if let Some(id) = task_id {
                    delete_task(&db, &sync_engine, id, state.clone()).await;
                }
            }
            KeyCode::Char('e') => {
                // Edit task title
                state.lock().unwrap().start_edit(EditField::Title);
            }
            KeyCode::Char('E') => {
                // Edit task description
                state.lock().unwrap().start_edit(EditField::Description);
            }
            KeyCode::Char('p') => {
                // Edit task priority
                state.lock().unwrap().start_edit(EditField::Priority);
            }
            KeyCode::Char('c') => {
                // Copy database path to clipboard
                let (_db_name, db_path) = {
                    let app_state = state.lock().unwrap();
                    let name = app_state.database_name.clone();
                    let path = format!("databases/{}.sqlite3", name);
                    (name, path)
                };

                // Try to copy to clipboard based on OS
                let copy_result = copy_to_clipboard(&db_path);

                // Add activity log entry
                {
                    let mut app_state = state.lock().unwrap();
                    if copy_result {
                        app_state.add_activity(
                            format!("Database path copied: {}", db_path),
                            ActivityType::SyncCompleted,
                        );
                    } else {
                        app_state.add_activity(
                            format!("Copy failed - path: {}", db_path),
                            ActivityType::Error,
                        );
                    }
                }
            }
            KeyCode::Enter => {
                // Enter edit mode for task title
                state.lock().unwrap().start_edit(EditField::Title);
            }
            _ => {}
        }
    }
}

async fn load_tasks(
    db: &ClientDatabase,
    user_id: Uuid,
    state: SharedState,
) -> Result<(), Box<dyn Error>> {
    // Log the query we're about to execute
    debug_log(&format!("Loading tasks for user_id: {}", user_id));

    let rows = sqlx::query(
        r#"
        SELECT id, content, sync_status, created_at, updated_at, sync_revision
        FROM documents
        WHERE user_id = ?1 AND deleted_at IS NULL
        ORDER BY created_at DESC
        "#,
    )
    .bind(user_id.to_string())
    .fetch_all(&db.pool)
    .await?;

    debug_log(&format!("Found {} documents", rows.len()));

    let mut tasks = Vec::new();
    for row in rows {
        let id = Uuid::parse_str(&row.try_get::<String, _>("id")?)?;
        let content_str = row.try_get::<String, _>("content")?;
        let sync_status = row.try_get::<Option<String>, _>("sync_status")?;
        let created_at = row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?;
        let updated_at = row.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?;
        let sync_revision = row.try_get::<i64, _>("sync_revision")?;

        let content: Value = serde_json::from_str(&content_str).unwrap_or_default();

        let task = Task {
            id,
            title: content
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Untitled")
                .to_string(),
            description: content
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            status: content
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending")
                .to_string(),
            priority: content
                .get("priority")
                .and_then(|v| v.as_str())
                .unwrap_or("medium")
                .to_string(),
            tags: content
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            created_at,
            updated_at,
            sync_revision,
            sync_status,
        };

        tasks.push(task);
    }

    {
        let mut app_state = state.lock().unwrap();
        app_state.tasks = tasks;
    }

    // Update sync status
    update_sync_status(db, state).await;

    Ok(())
}

async fn update_sync_status(db: &ClientDatabase, state: SharedState) {
    let pending =
        sqlx::query("SELECT COUNT(*) as count FROM documents WHERE sync_status = 'pending'")
            .fetch_one(&db.pool)
            .await
            .ok()
            .and_then(|row| row.try_get::<i64, _>("count").ok())
            .unwrap_or(0) as usize;

    let conflicts =
        sqlx::query("SELECT COUNT(*) as count FROM documents WHERE sync_status = 'conflict'")
            .fetch_one(&db.pool)
            .await
            .ok()
            .and_then(|row| row.try_get::<i64, _>("count").ok())
            .unwrap_or(0) as usize;

    let mut app_state = state.lock().unwrap();
    app_state.sync_status.pending_count = pending;
    app_state.sync_status.conflict_count = conflicts;
}

async fn toggle_task_completion(
    db: &ClientDatabase,
    sync_engine: &Arc<Mutex<Option<Arc<Client>>>>,
    task_id: Uuid,
    state: SharedState,
) {
    let doc = match db.get_document(&task_id).await {
        Ok(doc) => doc,
        Err(_) => return,
    };

    let mut content = doc.content.clone();
    if let Some(obj) = content.as_object_mut() {
        let current_status = obj.get("status").and_then(|v| v.as_str()).unwrap_or("todo");
        let new_status = if current_status == "completed" {
            "todo"
        } else {
            "completed"
        };
        obj.insert("status".to_string(), json!(new_status));

        if new_status == "completed" {
            obj.insert(
                "completed_at".to_string(),
                json!(chrono::Utc::now().to_rfc3339()),
            );
        } else {
            obj.remove("completed_at");
        }
    }

    let engine = sync_engine.lock().unwrap().clone();
    if let Some(engine) = engine {
        let _ = engine.update_document(task_id, content).await;
    } else {
        // Offline update
        let mut updated_doc = doc;
        updated_doc.content = content;
        updated_doc.sync_revision += 1;
        updated_doc.updated_at = chrono::Utc::now();

        let _ = db.save_document(&updated_doc).await;

        // Mark as pending sync
        let _ = sqlx::query("UPDATE documents SET sync_status = 'pending' WHERE id = ?1")
            .bind(task_id.to_string())
            .execute(&db.pool)
            .await;
    }

    // Trigger UI refresh
    {
        let mut app_state = state.lock().unwrap();
        app_state.needs_refresh = true;
    }

    // Also reload tasks immediately for responsive UI
    if let Ok(user_id) = db.get_user_id().await {
        if let Err(e) = load_tasks(db, user_id, state.clone()).await {
            let mut app_state = state.lock().unwrap();
            app_state.add_activity(
                format!("Failed to reload tasks: {}", e),
                ActivityType::Error,
            );
        }
    }
}

async fn create_sample_task(
    db: &ClientDatabase,
    sync_engine: &Arc<Mutex<Option<Arc<Client>>>>,
    user_id: Uuid,
    state: SharedState,
) {
    let title = format!("New Task {}", chrono::Utc::now().format("%H:%M:%S"));
    let content = json!({
        "title": title.clone(),
        "description": "Created from task list UI",
        "status": "todo",
        "priority": "medium",
        "tags": vec!["ui", "demo"],
        "created_at": chrono::Utc::now().to_rfc3339(),
    });

    let engine = sync_engine.lock().unwrap().clone();
    if let Some(engine) = engine {
        let _ = engine.create_document(content).await;
    } else {
        // Offline create
        let doc = Document {
            id: Uuid::new_v4(),
            user_id,
            content: content.clone(),
            sync_revision: 1,
            content_hash: None,
            title: content
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
        };

        let _ = db.save_document(&doc).await;
    }

    // Activity will be logged via event callback if online, or we can add it manually if offline
    if sync_engine.lock().unwrap().is_none() {
        let mut app_state = state.lock().unwrap();
        app_state.add_activity("Task created (offline)".to_string(), ActivityType::Created);
    }

    // Trigger UI refresh for immediate feedback
    {
        let mut app_state = state.lock().unwrap();
        app_state.needs_refresh = true;
    }
}

async fn delete_task(
    db: &ClientDatabase,
    sync_engine: &Arc<Mutex<Option<Arc<Client>>>>,
    task_id: Uuid,
    state: SharedState,
) {
    let engine = sync_engine.lock().unwrap().clone();
    if let Some(engine) = engine {
        // Use sync engine if available
        let _ = engine.delete_document(task_id).await;

        let mut app_state = state.lock().unwrap();
        app_state.add_activity("Task deleted".to_string(), ActivityType::Deleted);
    } else {
        // Offline delete
        let _ = db.delete_document(&task_id).await;

        let mut app_state = state.lock().unwrap();
        app_state.add_activity("Task deleted (offline)".to_string(), ActivityType::Deleted);
    }

    // Trigger UI refresh for immediate feedback
    {
        let mut app_state = state.lock().unwrap();
        app_state.needs_refresh = true;
    }
}

async fn save_task_edit(
    db: &ClientDatabase,
    sync_engine: &Arc<Mutex<Option<Arc<Client>>>>,
    edit: EditMode,
    state: SharedState,
) {
    // Get the current document
    let doc = match db.get_document(&edit.task_id).await {
        Ok(doc) => doc,
        Err(_) => return,
    };

    // Create updated content
    let mut content = doc.content.clone();
    if let Some(obj) = content.as_object_mut() {
        match edit.field {
            EditField::Title => {
                obj.insert("title".to_string(), json!(edit.input));
            }
            EditField::Description => {
                obj.insert("description".to_string(), json!(edit.input));
            }
            EditField::Priority => {
                // Use the priority enum value
                obj.insert("priority".to_string(), json!(edit.priority.to_string()));
            }
        }
    }

    // Update the document
    let engine = sync_engine.lock().unwrap().clone();
    if let Some(engine) = engine {
        let _ = engine.update_document(edit.task_id, content).await;
    } else {
        // Offline update
        let mut updated_doc = doc;
        updated_doc.content = content;
        updated_doc.sync_revision += 1;
        updated_doc.updated_at = chrono::Utc::now();

        let _ = db.save_document(&updated_doc).await;

        // Mark as pending sync
        let _ = sqlx::query("UPDATE documents SET sync_status = 'pending' WHERE id = ?1")
            .bind(edit.task_id.to_string())
            .execute(&db.pool)
            .await;
    }

    // Add activity log
    {
        let mut app_state = state.lock().unwrap();
        let field_name = match edit.field {
            EditField::Title => "title",
            EditField::Description => "description",
            EditField::Priority => "priority",
        };
        app_state.add_activity(
            format!("Updated task {}", field_name),
            ActivityType::Updated,
        );
        app_state.needs_refresh = true;
    }

    // Also reload tasks immediately for responsive UI
    if let Ok(user_id) = db.get_user_id().await {
        if let Err(e) = load_tasks(db, user_id, state.clone()).await {
            let mut app_state = state.lock().unwrap();
            app_state.add_activity(
                format!("Failed to reload tasks: {}", e),
                ActivityType::Error,
            );
        }
    }
}

async fn setup_user(
    db: &ClientDatabase,
    user_id: Uuid,
    client_id: Uuid,
    server_url: &str,
    _api_key: &str,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("INSERT INTO user_config (user_id, client_id, server_url) VALUES (?1, ?2, ?3)")
        .bind(user_id.to_string())
        .bind(client_id.to_string())
        .bind(server_url)
        .execute(&db.pool)
        .await?;
    Ok(())
}

fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};

        match Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    if stdin.write_all(text.as_bytes()).is_ok() {
                        drop(stdin);
                        return child.wait().map(|s| s.success()).unwrap_or(false);
                    }
                }
                false
            }
            Err(_) => false,
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // Try xclip first
        if let Ok(mut child) = Command::new("xclip")
            .arg("-selection")
            .arg("clipboard")
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                if stdin.write_all(text.as_bytes()).is_ok() {
                    drop(stdin);
                    return child.wait().map(|s| s.success()).unwrap_or(false);
                }
            }
        }

        // Try xsel as fallback
        if let Ok(mut child) = Command::new("xsel")
            .arg("--clipboard")
            .arg("--input")
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                if stdin.write_all(text.as_bytes()).is_ok() {
                    drop(stdin);
                    return child.wait().map(|s| s.success()).unwrap_or(false);
                }
            }
        }

        false
    }

    #[cfg(target_os = "windows")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};

        match Command::new("cmd")
            .args(&["/C", "clip"])
            .stdin(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    if stdin.write_all(text.as_bytes()).is_ok() {
                        drop(stdin);
                        return child.wait().map(|s| s.success()).unwrap_or(false);
                    }
                }
                false
            }
            Err(_) => false,
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}
