use chrono::Utc;
use clap::Parser;
use colored::*;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use replicant_client::{Client, ClientDatabase};
use replicant_core::models::Document;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "sync-client")]
#[command(about = "Interactive JSON database sync client", long_about = None)]
struct Cli {
    /// Database file name (will auto-create in databases/ directory)
    #[arg(short, long, default_value = "alice")]
    database: String,

    /// Server WebSocket URL
    #[arg(short, long, default_value = "ws://localhost:8080/ws")]
    server: String,

    /// API key for authentication
    #[arg(
        short = 'k',
        long,
        default_value = "rpa_demo123456789012345678901234567890"
    )]
    api_key: String,

    /// API secret for authentication
    #[arg(
        short = 'k',
        long,
        default_value = "rpa_demo123456789012345678901234567890"
    )]
    api_secret: String,

    /// User ID (will be generated if not provided)
    #[arg(short, long)]
    user_id: Option<String>,

    /// Email
    #[arg(short, long, default_value = "example@gmail.com")]
    email: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging (only show warnings and errors)
    tracing_subscriber::fmt().with_env_filter("warn").init();

    let cli = Cli::parse();

    // Auto-create database file path with .sqlite3 extension in databases/ folder
    std::fs::create_dir_all("databases")?;
    let db_file = format!("databases/{}.sqlite3", cli.database);
    let db_url = format!("sqlite:{}?mode=rwc", db_file);

    println!("{}", "🚀 JSON Database Sync Client".bold().cyan());
    println!("{}", "============================".cyan());
    println!("📁 Database: {}", db_file.green());

    // Initialize database (will auto-create the file)
    let db = ClientDatabase::new(&db_url).await?;

    // Run migrations
    db.run_migrations().await?;

    // Get or create user
    let user_id = match cli.user_id {
        Some(id) => Uuid::parse_str(&id)?,
        None => match db.get_user_id().await {
            Ok(id) => id,
            Err(_) => {
                let id = Uuid::new_v4();
                println!("🆕 Creating new user: {}", id.to_string().yellow());
                let client_id = Uuid::new_v4();
                setup_user(&db, id, client_id, &cli.server, &cli.api_key).await?;
                id
            }
        },
    };

    println!("👤 User ID: {}", user_id.to_string().green());
    println!("🌐 Server: {}", cli.server.blue());
    println!();

    // Create sync engine (auto-starts with built-in reconnection)
    let sync_engine = match Client::new(
        &db_url,
        &cli.server,
        &cli.email,
        &cli.api_key,
        &cli.api_secret,
    )
    .await
    {
        Ok(engine) => {
            println!("✅ Sync engine initialized (auto-connecting)!");
            Some(engine)
        }
        Err(e) => {
            println!(
                "⚠️  Failed to initialize sync engine: {}",
                e.to_string().yellow()
            );
            None
        }
    };

    // Main interactive loop
    loop {
        let choices = vec![
            "📋 List tasks",
            "➕ Create new task",
            "✏️  Edit task",
            "🔍 View task details",
            "✅ Mark task completed",
            "🗑️  Delete task",
            "🔄 Sync status",
            "❌ Exit",
        ];

        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("What would you like to do?")
            .items(&choices)
            .default(0)
            .interact()?;

        match selection {
            0 => list_tasks(&db, user_id).await?,
            1 => create_task(&db, &sync_engine, user_id).await?,
            2 => edit_task(&db, &sync_engine, user_id).await?,
            3 => view_task(&db, user_id).await?,
            4 => complete_task(&db, &sync_engine, user_id).await?,
            5 => delete_task(&db, &sync_engine, user_id).await?,
            6 => show_sync_status(&db).await?,
            7 => {
                if Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Are you sure you want to exit?")
                    .default(false)
                    .interact()?
                {
                    println!("👋 Goodbye!");
                    break;
                }
            }
            _ => unreachable!(),
        }
        println!();
    }

    Ok(())
}

async fn setup_user(
    db: &ClientDatabase,
    user_id: Uuid,
    client_id: Uuid,
    server_url: &str,
    _api_key: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO user_config (user_id, client_id, server_url) VALUES (?1, ?2, ?3)")
        .bind(user_id.to_string())
        .bind(client_id.to_string())
        .bind(server_url)
        .execute(&db.pool)
        .await?;
    Ok(())
}

async fn list_tasks(db: &ClientDatabase, user_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
    let rows = sqlx::query(
        r#"
        SELECT id, title, content, sync_status, updated_at 
        FROM documents 
        WHERE user_id = ?1 AND deleted_at IS NULL
        ORDER BY updated_at DESC
        "#,
    )
    .bind(user_id.to_string())
    .fetch_all(&db.pool)
    .await?;

    if rows.is_empty() {
        println!("📭 No tasks found.");
    } else {
        println!("{}", "📋 Your Tasks:".bold());
        println!("{}", "─".repeat(100).dimmed());

        for row in rows {
            let id = row.try_get::<String, _>("id")?;
            let title = row.try_get::<String, _>("title")?;
            let content_str = row.try_get::<String, _>("content")?;
            let sync_status = row.try_get::<Option<String>, _>("sync_status")?;
            let _updated_at = row.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?;

            let content: Value = serde_json::from_str(&content_str).unwrap_or_default();
            let task_status = content
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");
            let priority = content
                .get("priority")
                .and_then(|s| s.as_str())
                .unwrap_or("medium");
            let description = content
                .get("description")
                .and_then(|s| s.as_str())
                .unwrap_or("");

            let status_icon = match task_status {
                "completed" => "✅",
                "in_progress" => "🔄",
                "pending" => "⏳",
                _ => "❓",
            };

            let priority_icon = match priority {
                "high" => "🔴",
                "medium" => "🟡",
                "low" => "🟢",
                _ => "⚪",
            };

            let sync_icon = match sync_status.as_deref() {
                Some("synced") => "",
                Some("pending") => "📤",
                Some("conflict") => "⚠️",
                _ => "📱",
            };

            println!(
                "{} {} {} {} {} {}",
                status_icon,
                priority_icon,
                id[..8].blue(),
                title.white().bold(),
                if !description.is_empty() {
                    format!("- {}", description.dimmed())
                } else {
                    "".to_string()
                },
                sync_icon
            );
        }
        println!("{}", "─".repeat(100).dimmed());
        println!(
            "{}",
            "Legend: ✅=done 🔄=progress ⏳=pending | 🔴=high 🟡=med 🟢=low | 📤=sync pending"
                .dimmed()
        );
    }

    Ok(())
}

async fn create_task(
    db: &ClientDatabase,
    sync_engine: &Option<Client>,
    user_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", "📝 Create New Task".bold().cyan());
    println!("{}", "─".repeat(40).dimmed());

    let title: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Task title")
        .interact_text()?;

    let description: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Description")
        .default("".to_string())
        .interact_text()?;

    let priority_choices = vec!["low", "medium", "high"];
    let priority_selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Priority")
        .items(&priority_choices)
        .default(1)
        .interact()?;
    let priority = priority_choices[priority_selection];

    let tags_input: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Tags (comma-separated, optional)")
        .default("".to_string())
        .interact_text()?;

    let tags: Vec<String> = if tags_input.trim().is_empty() {
        vec![]
    } else {
        tags_input
            .split(',')
            .map(|s| s.trim().to_string())
            .collect()
    };

    let content = json!({
        "title": title.clone(),
        "description": description,
        "status": "pending",
        "priority": priority,
        "tags": tags,
        "created_at": Utc::now().to_rfc3339(),
        "due_date": null
    });

    if let Some(engine) = sync_engine {
        // Use sync engine if connected
        let mut full_content = content.clone();
        full_content["title"] = serde_json::json!(title);
        let doc = engine.create_document(full_content).await?;
        println!("✅ Task created: {}", doc.id.to_string().green());
    } else {
        // Offline mode - create locally
        let mut full_content = content.clone();
        full_content["title"] = serde_json::json!(title);
        let doc = Document {
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
        println!("✅ Task created (offline): {}", doc.id.to_string().yellow());
    }

    Ok(())
}

async fn complete_task(
    db: &ClientDatabase,
    sync_engine: &Option<Client>,
    user_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    // List pending tasks for selection
    let rows = sqlx::query(
        r#"
        SELECT id, title, content
        FROM documents 
        WHERE user_id = ?1 AND deleted_at IS NULL
        ORDER BY updated_at DESC
        "#,
    )
    .bind(user_id.to_string())
    .fetch_all(&db.pool)
    .await?;

    let pending_tasks: Vec<_> = rows
        .into_iter()
        .filter_map(|row| {
            let id = row.try_get::<String, _>("id").ok()?;
            let title = row.try_get::<String, _>("title").ok()?;
            let content_str = row.try_get::<String, _>("content").ok()?;
            let content: Value = serde_json::from_str(&content_str).ok()?;
            let status = content.get("status")?.as_str()?;

            if status != "completed" {
                Some((id, title, content))
            } else {
                None
            }
        })
        .collect();

    if pending_tasks.is_empty() {
        println!("✅ All tasks are already completed!");
        return Ok(());
    }

    let choices: Vec<String> = pending_tasks
        .iter()
        .map(|(id, title, content)| {
            let _status = content
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("pending");
            let priority = content
                .get("priority")
                .and_then(|s| s.as_str())
                .unwrap_or("medium");
            let priority_icon = match priority {
                "high" => "🔴",
                "medium" => "🟡",
                "low" => "🟢",
                _ => "⚪",
            };
            format!("{} {} - {}", priority_icon, &id[..8], title)
        })
        .collect();

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select task to mark as completed")
        .items(&choices)
        .interact()?;

    let (task_id, task_title, task_content) = &pending_tasks[selection];
    let mut task_content = task_content.clone();
    let doc_id = Uuid::parse_str(task_id)?;

    // Update the task content to mark as completed
    if let Some(obj) = task_content.as_object_mut() {
        obj.insert("status".to_string(), json!("completed"));
        obj.insert("completed_at".to_string(), json!(Utc::now().to_rfc3339()));
    }

    if let Some(engine) = sync_engine {
        engine.update_document(doc_id, task_content.clone()).await?;
        println!(
            "✅ Task '{}' marked as completed and synced!",
            task_title.green()
        );
    } else {
        // Offline update
        let doc = db.get_document(&doc_id).await?;
        let mut updated_doc = doc;
        updated_doc.content = task_content.clone();
        updated_doc.sync_revision += 1;
        updated_doc.updated_at = chrono::Utc::now();

        db.save_document(&updated_doc).await?;

        // Mark as pending sync
        sqlx::query("UPDATE documents SET sync_status = 'pending' WHERE id = ?1")
            .bind(doc_id.to_string())
            .execute(&db.pool)
            .await?;

        println!(
            "✅ Task '{}' marked as completed (offline)",
            task_title.yellow()
        );
    }

    Ok(())
}

async fn edit_task(
    db: &ClientDatabase,
    sync_engine: &Option<Client>,
    user_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    // List tasks for selection
    let rows = sqlx::query(
        r#"
        SELECT id, title, content
        FROM documents 
        WHERE user_id = ?1 AND deleted_at IS NULL
        ORDER BY updated_at DESC
        "#,
    )
    .bind(user_id.to_string())
    .fetch_all(&db.pool)
    .await?;

    if rows.is_empty() {
        println!("📭 No tasks to edit.");
        return Ok(());
    }

    let mut task_info = Vec::new();
    let mut choices = Vec::new();

    for row in rows {
        let id = row.try_get::<String, _>("id")?;
        let title = row.try_get::<String, _>("title")?;
        let content_str = row.try_get::<String, _>("content")?;
        let content: Value = serde_json::from_str(&content_str).unwrap_or_default();
        let status = content
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending");
        let priority = content
            .get("priority")
            .and_then(|s| s.as_str())
            .unwrap_or("medium");

        let status_icon = match status {
            "completed" => "✅",
            "in_progress" => "🔄",
            "pending" => "⏳",
            _ => "❓",
        };
        let priority_icon = match priority {
            "high" => "🔴",
            "medium" => "🟡",
            "low" => "🟢",
            _ => "⚪",
        };

        task_info.push((id.clone(), content));
        choices.push(format!(
            "{} {} {} - {}",
            status_icon,
            priority_icon,
            &id[..8],
            title
        ));
    }

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select task to edit")
        .items(&choices)
        .interact()?;

    let doc_id = Uuid::parse_str(&task_info[selection].0)?;
    let current_content = task_info[selection].1.clone();

    println!("{}", "✏️  Edit Task".bold().cyan());
    println!("{}", "─".repeat(40).dimmed());

    // Edit title
    let current_title = current_content
        .get("title")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let new_title: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Title")
        .default(current_title.to_string())
        .interact_text()?;

    // Edit description
    let current_description = current_content
        .get("description")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let new_description: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Description")
        .default(current_description.to_string())
        .interact_text()?;

    // Edit status
    let status_choices = vec!["pending", "in_progress", "completed"];
    let current_status = current_content
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("pending");
    let current_status_index = status_choices
        .iter()
        .position(|&s| s == current_status)
        .unwrap_or(0);
    let status_selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Status")
        .items(&status_choices)
        .default(current_status_index)
        .interact()?;

    // Edit priority
    let priority_choices = vec!["low", "medium", "high"];
    let current_priority = current_content
        .get("priority")
        .and_then(|s| s.as_str())
        .unwrap_or("medium");
    let current_priority_index = priority_choices
        .iter()
        .position(|&s| s == current_priority)
        .unwrap_or(1);
    let priority_selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Priority")
        .items(&priority_choices)
        .default(current_priority_index)
        .interact()?;

    // Edit tags
    let current_tags = current_content
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let tags_input: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Tags (comma-separated)")
        .default(current_tags)
        .interact_text()?;

    let tags: Vec<String> = if tags_input.trim().is_empty() {
        vec![]
    } else {
        tags_input
            .split(',')
            .map(|s| s.trim().to_string())
            .collect()
    };

    // Create updated content
    let mut new_content = current_content.clone();
    if let Some(obj) = new_content.as_object_mut() {
        obj.insert("title".to_string(), json!(new_title.clone()));
        obj.insert("description".to_string(), json!(new_description));
        obj.insert(
            "status".to_string(),
            json!(status_choices[status_selection]),
        );
        obj.insert(
            "priority".to_string(),
            json!(priority_choices[priority_selection]),
        );
        obj.insert("tags".to_string(), json!(tags));

        if status_choices[status_selection] == "completed" && current_status != "completed" {
            obj.insert("completed_at".to_string(), json!(Utc::now().to_rfc3339()));
        }
    }

    if let Some(engine) = sync_engine {
        engine.update_document(doc_id, new_content).await?;
        println!("✅ Task updated and synced!");
    } else {
        // Offline update
        let doc = db.get_document(&doc_id).await?;
        let mut updated_doc = doc;
        let mut full_content = new_content.clone();
        full_content["title"] = serde_json::json!(new_title);
        updated_doc.content = full_content;
        updated_doc.sync_revision += 1;
        updated_doc.updated_at = chrono::Utc::now();

        db.save_document(&updated_doc).await?;

        // Mark as pending sync
        sqlx::query("UPDATE documents SET sync_status = 'pending' WHERE id = ?1")
            .bind(doc_id.to_string())
            .execute(&db.pool)
            .await?;

        println!("✅ Task updated (offline)");
    }

    Ok(())
}

async fn view_task(db: &ClientDatabase, user_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
    let rows = sqlx::query(
        r#"
        SELECT id, title, content, created_at, updated_at, version
        FROM documents 
        WHERE user_id = ?1 AND deleted_at IS NULL
        ORDER BY updated_at DESC
        "#,
    )
    .bind(user_id.to_string())
    .fetch_all(&db.pool)
    .await?;

    if rows.is_empty() {
        println!("📭 No tasks to view.");
        return Ok(());
    }

    let mut task_info = Vec::new();
    let mut choices = Vec::new();

    for row in rows {
        let id = row.try_get::<String, _>("id")?;
        let title = row.try_get::<String, _>("title")?;
        let content_str = row.try_get::<String, _>("content")?;
        let content: Value = serde_json::from_str(&content_str).unwrap_or_default();
        let status = content
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending");
        let priority = content
            .get("priority")
            .and_then(|s| s.as_str())
            .unwrap_or("medium");

        let status_icon = match status {
            "completed" => "✅",
            "in_progress" => "🔄",
            "pending" => "⏳",
            _ => "❓",
        };
        let priority_icon = match priority {
            "high" => "🔴",
            "medium" => "🟡",
            "low" => "🟢",
            _ => "⚪",
        };

        task_info.push(row);
        choices.push(format!(
            "{} {} {} - {}",
            status_icon,
            priority_icon,
            &id[..8],
            title
        ));
    }

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select task to view")
        .items(&choices)
        .interact()?;

    let row = &task_info[selection];
    let id = row.try_get::<String, _>("id")?;
    let title = row.try_get::<String, _>("title")?;
    let content_str = row.try_get::<String, _>("content")?;
    let created_at = row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?;
    let updated_at = row.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?;
    let version = row.try_get::<i64, _>("version")?;

    let content: Value = serde_json::from_str(&content_str)?;

    println!("{}", "📋 Task Details".bold());
    println!("{}", "═".repeat(60).dimmed());
    println!("📌 ID:          {}", id.blue());
    println!("📝 Title:       {}", title.white().bold());

    if let Some(description) = content.get("description").and_then(|s| s.as_str()) {
        if !description.is_empty() {
            println!("📄 Description: {}", description);
        }
    }

    if let Some(status) = content.get("status").and_then(|s| s.as_str()) {
        let status_display = match status {
            "completed" => "✅ Completed".green(),
            "in_progress" => "🔄 In Progress".yellow(),
            "pending" => "⏳ Pending".blue(),
            _ => status.dimmed(),
        };
        println!("📊 Status:      {}", status_display);
    }

    if let Some(priority) = content.get("priority").and_then(|s| s.as_str()) {
        let priority_display = match priority {
            "high" => "🔴 High".red(),
            "medium" => "🟡 Medium".yellow(),
            "low" => "🟢 Low".green(),
            _ => priority.dimmed(),
        };
        println!("⚡ Priority:    {}", priority_display);
    }

    if let Some(tags) = content.get("tags").and_then(|t| t.as_array()) {
        let tag_strings: Vec<String> = tags
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| format!("#{}", s))
            .collect();
        if !tag_strings.is_empty() {
            println!("🏷️  Tags:        {}", tag_strings.join(" ").cyan());
        }
    }

    if let Some(created_str) = content.get("created_at").and_then(|s| s.as_str()) {
        println!("🕐 Created:     {}", created_str.dimmed());
    } else {
        println!("🕐 Created:     {}", created_at.to_string().dimmed());
    }

    if let Some(completed_str) = content.get("completed_at").and_then(|s| s.as_str()) {
        println!("✅ Completed:   {}", completed_str.green());
    }

    println!("🔄 Updated:     {}", updated_at.to_string().dimmed());
    println!("📊 Version:     {}", version.to_string().yellow());
    println!("{}", "═".repeat(60).dimmed());

    Ok(())
}

async fn delete_task(
    db: &ClientDatabase,
    sync_engine: &Option<Client>,
    user_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    // List tasks for selection
    let rows = sqlx::query(
        r#"
        SELECT id, title, content
        FROM documents 
        WHERE user_id = ?1 AND deleted_at IS NULL
        ORDER BY updated_at DESC
        "#,
    )
    .bind(user_id.to_string())
    .fetch_all(&db.pool)
    .await?;

    if rows.is_empty() {
        println!("📭 No tasks to delete.");
        return Ok(());
    }

    let mut task_info = Vec::new();
    let mut choices = Vec::new();

    for row in rows {
        let id = row.try_get::<String, _>("id")?;
        let title = row.try_get::<String, _>("title")?;
        let content_str = row.try_get::<String, _>("content")?;
        let content: Value = serde_json::from_str(&content_str).unwrap_or_default();
        let status = content
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending");
        let priority = content
            .get("priority")
            .and_then(|s| s.as_str())
            .unwrap_or("medium");

        let status_icon = match status {
            "completed" => "✅",
            "in_progress" => "🔄",
            "pending" => "⏳",
            _ => "❓",
        };
        let priority_icon = match priority {
            "high" => "🔴",
            "medium" => "🟡",
            "low" => "🟢",
            _ => "⚪",
        };

        task_info.push((id.clone(), title.clone()));
        choices.push(format!(
            "{} {} {} - {}",
            status_icon,
            priority_icon,
            &id[..8],
            title
        ));
    }

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select task to delete")
        .items(&choices)
        .interact()?;

    let task_id = Uuid::parse_str(&task_info[selection].0)?;
    let task_title = &task_info[selection].1;

    // Confirm deletion
    if !Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(&format!(
            "⚠️  Are you sure you want to delete task '{}'? This action cannot be undone.",
            task_title
        ))
        .default(false)
        .interact()?
    {
        println!("❌ Deletion cancelled.");
        return Ok(());
    }

    if let Some(engine) = sync_engine {
        engine.delete_document(task_id).await?;
        println!("✅ Task '{}' deleted and synced!", task_title.green());
    } else {
        // Offline delete - just mark as deleted locally
        db.delete_document(&task_id).await?;
        println!("✅ Task '{}' deleted (offline)", task_title.yellow());
    }

    Ok(())
}

async fn show_sync_status(db: &ClientDatabase) -> Result<(), Box<dyn std::error::Error>> {
    let pending_row =
        sqlx::query("SELECT COUNT(*) as count FROM documents WHERE sync_status = 'pending'")
            .fetch_one(&db.pool)
            .await?;
    let pending_count = pending_row.try_get::<i64, _>("count")?;

    let conflict_row =
        sqlx::query("SELECT COUNT(*) as count FROM documents WHERE sync_status = 'conflict'")
            .fetch_one(&db.pool)
            .await?;
    let conflict_count = conflict_row.try_get::<i64, _>("count")?;

    println!("{}", "🔄 Sync Status".bold());
    println!("{}", "─".repeat(40).dimmed());
    println!("⏳ Pending sync:  {}", pending_count.to_string().yellow());
    println!("⚠️  Conflicts:     {}", conflict_count.to_string().red());
    println!("{}", "─".repeat(40).dimmed());

    Ok(())
}
