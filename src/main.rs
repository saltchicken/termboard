mod config;
use crate::config::setup_config;
use crossterm::{
    event::{
        self,
        Event,
        EventStream,
        KeyCode,
        KeyEventKind,
        KeyModifiers, // ‼️ Removed MouseEvent imports
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use futures_util::StreamExt;
use ratatui::{
    prelude::*,
    widgets::{
        canvas::{self, Canvas, Context, Line as CanvasLine, Rectangle},
        Block, Borders, Clear, List, ListItem, ListState, Paragraph,
    },
};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use std::{
    collections::HashMap,
    io::{self, Stdout},
    time::{Duration, Instant},
};

/// We need tokio's runtime for the async event loop
#[tokio::main]
async fn main() -> io::Result<()> {
    let config = setup_config()?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e.to_string()))?;

    // --- Database Setup (Identical to previous) ---
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS boards (
            id BIGSERIAL PRIMARY KEY,
            name TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(&pool)
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let default_exists: bool = sqlx::query("SELECT EXISTS(SELECT 1 FROM boards)")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);

    if !default_exists {
        sqlx::query("INSERT INTO boards (id, name) VALUES (1, 'Default Board')")
            .execute(&pool)
            .await
            .ok();
    }

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS shapes (
            id BIGINT PRIMARY KEY,
            kind TEXT NOT NULL,
            x DOUBLE PRECISION NOT NULL,
            y DOUBLE PRECISION NOT NULL,
            width DOUBLE PRECISION NOT NULL,
            height DOUBLE PRECISION NOT NULL,
            label TEXT NOT NULL,
            color TEXT NOT NULL,
            board_id BIGINT NOT NULL DEFAULT 1
        )
        "#,
    )
    .execute(&pool)
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS connections (
            id SERIAL PRIMARY KEY,
            id_a BIGINT NOT NULL,
            id_b BIGINT NOT NULL,
            board_id BIGINT NOT NULL DEFAULT 1
        )
        "#,
    )
    .execute(&pool)
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let _ = sqlx::query(
        "ALTER TABLE shapes ADD COLUMN IF NOT EXISTS board_id BIGINT NOT NULL DEFAULT 1",
    )
    .execute(&pool)
    .await;

    let _ = sqlx::query(
        "ALTER TABLE connections ADD COLUMN IF NOT EXISTS board_id BIGINT NOT NULL DEFAULT 1",
    )
    .execute(&pool)
    .await;

    let _ = sqlx::query(
        r#"
        DO $$
        BEGIN
            IF EXISTS (
                SELECT 1 FROM pg_constraint WHERE conname = 'shapes_pkey'
            ) THEN
                ALTER TABLE shapes DROP CONSTRAINT shapes_pkey;
                ALTER TABLE shapes ADD PRIMARY KEY (board_id, id);
            END IF;
        END $$;
        "#,
    )
    .execute(&pool)
    .await;

    // --- TUI Setup ---
    let mut terminal = Tui::new()?;

    // --- App State ---
    let mut app = App::new(pool);
    app.refresh_board_list().await;
    app.load_state().await;

    let tick_rate = Duration::from_millis(33); // ~30 FPS
    let mut last_tick = Instant::now();
    let mut event_stream = EventStream::new();

    // --- Main Loop ---
    loop {
        if app.should_quit {
            break;
        }

        // Draw the UI
        terminal.draw(|frame| app.ui(frame))?;

        // Handle input events
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        // Poll for event
        if crossterm::event::poll(timeout)? {
            match event_stream.next().await {
                Some(Ok(Event::Key(key))) => {
                    // ‼️ Ensure we only trigger on Press to avoid some key repeat issues
                    if key.kind == KeyEventKind::Press {
                        app.handle_key_event(key).await;
                    }
                }
                Some(Ok(Event::Resize(width, height))) => app.on_resize(width, height),
                // ‼️ Removed Mouse event matching here
                _ => {}
            }
        }

        // Tick logic
        if last_tick.elapsed() >= tick_rate {
            app.on_tick();
            last_tick = Instant::now();
        }
    }

    // Restore terminal
    terminal.exit()?;
    Ok(())
}

// --- App State and Logic ---

#[derive(PartialEq, Eq, Clone, Copy, Default)]
enum Tool {
    #[default]
    Pointer,
    // ‼️ DrawRect removed (replaced by 'N' hotkey)
    Connect,
}

#[derive(PartialEq, Eq, Clone, Copy, Default)]
enum Mode {
    #[default]
    Normal,
    Editing,
    BoardMenu,
}

// ‼️ Removed ResizeHandle enum (resizing is now done via Ctrl+Arrow keys)

#[derive(Clone, Copy)]
enum ShapeKind {
    Rectangle,
}

#[derive(Clone)]
struct WhiteboardShape {
    id: u64,
    kind: ShapeKind,
    /// We use the canvas::Rectangle for f64-based coordinates
    rect: canvas::Rectangle,
    label: String,
    color: Color,
}

impl WhiteboardShape {
    /// Gets the center position of the shape (in world coords)
    fn center(&self) -> (f64, f64) {
        (
            self.rect.x + self.rect.width / 2.0,
            self.rect.y + self.rect.height / 2.0,
        )
    }

    fn get_boundary_point(&self, target: (f64, f64)) -> (f64, f64) {
        let (cx, cy) = self.center();
        let (tx, ty) = target;
        let dx = tx - cx;
        let dy = ty - cy;

        if dx == 0.0 && dy == 0.0 {
            return (cx, cy);
        }

        let half_w = self.rect.width / 2.0;
        let half_h = self.rect.height / 2.0;

        let scale_x = if dx == 0.0 {
            f64::INFINITY
        } else {
            half_w / dx.abs()
        };

        let scale_y = if dy == 0.0 {
            f64::INFINITY
        } else {
            half_h / dy.abs()
        };

        let scale = scale_x.min(scale_y);
        (cx + dx * scale, cy + dy * scale)
    }

    // ‼️ Removed translate/resize helper methods (direct manipulation in App is simpler for keyboard)

    /// Checks if a world coordinate (x, y) is inside this shape
    fn contains(&self, x: f64, y: f64) -> bool {
        match self.kind {
            ShapeKind::Rectangle => {
                (x >= self.rect.x && x <= (self.rect.x + self.rect.width))
                    && (y >= self.rect.y && y <= (self.rect.y + self.rect.height))
            }
        }
    }

    // ‼️ Removed get_handle_collision (no mouse handles)
}

/// Represents a connection between two shapes, identified by their IDs.
struct Connection {
    id_a: u64,
    id_b: u64,
}

struct BoardInfo {
    id: i64,
    name: String,
}

/// Holds the entire state of our application.
struct App {
    pool: PgPool,
    shapes: HashMap<u64, WhiteboardShape>,
    connections: Vec<Connection>,
    active_tool: Tool,
    mode: Mode,
    current_board_id: i64,
    current_board_name: String,
    available_boards: Vec<BoardInfo>,
    board_list_state: ListState,
    new_board_input: String,

    // ‼️ NEW: Keyboard Cursor Logic
    cursor_pos: (f64, f64),
    move_speed: f64,

    // ‼️ Removed mouse drag/pan state fields
    /// ID of the currently selected shape.
    selected_shape_id: Option<u64>,

    label_edit_buffer: String,
    connect_start_id: Option<u64>,
    next_id: u64,

    /// Pan offset (top-left corner of the view in world coords)
    pan_offset: (f64, f64),
    /// View dimensions in world coords
    view_size: (f64, f64),
    /// The Rect of the terminal area allocated to the canvas
    canvas_area: Rect,

    should_quit: bool,
    status_msg: String,
}

impl App {
    fn new(pool: PgPool) -> Self {
        Self {
            pool,
            shapes: HashMap::new(),
            connections: Vec::new(),
            active_tool: Tool::Pointer,
            mode: Mode::Normal,
            current_board_id: 1,
            current_board_name: "Default".to_string(),
            available_boards: Vec::new(),
            board_list_state: ListState::default(),
            new_board_input: String::new(),

            // ‼️ Initialize cursor in middle of default view
            cursor_pos: (100.0, 50.0),
            move_speed: 2.0,

            selected_shape_id: None,
            label_edit_buffer: String::new(),
            connect_start_id: None,
            next_id: 0,
            pan_offset: (0.0, 0.0),
            view_size: (200.0, 100.0),
            canvas_area: Rect::default(),
            should_quit: false,
            status_msg: String::from("Ready. Use Arrows to move cursor."),
        }
    }

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    async fn refresh_board_list(&mut self) {
        let rows = sqlx::query("SELECT id, name FROM boards ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        self.available_boards = rows
            .into_iter()
            .map(|r| BoardInfo {
                id: r.get("id"),
                name: r.get("name"),
            })
            .collect();

        if self.board_list_state.selected().is_none() && !self.available_boards.is_empty() {
            self.board_list_state.select(Some(0));
        }
    }

    async fn create_board(&mut self, name: &str) {
        if name.trim().is_empty() {
            return;
        }
        let res = sqlx::query("INSERT INTO boards (name) VALUES ($1) RETURNING id")
            .bind(name)
            .fetch_one(&self.pool)
            .await;

        match res {
            Ok(row) => {
                let new_id: i64 = row.get("id");
                self.current_board_id = new_id;
                self.current_board_name = name.to_string();
                self.refresh_board_list().await;
                self.shapes.clear();
                self.connections.clear();
                self.next_id = 0;
                self.status_msg = format!("Created Board '{}'", name);
                self.mode = Mode::Normal;
            }
            Err(e) => self.status_msg = format!("Error creating board: {}", e),
        }
    }

    async fn save_state(&mut self) {
        self.status_msg = "Saving...".to_string();
        let mut tx = match self.pool.begin().await {
            Ok(t) => t,
            Err(e) => {
                self.status_msg = format!("DB Error: {}", e);
                return;
            }
        };

        let _ = sqlx::query("DELETE FROM connections WHERE board_id = $1")
            .bind(self.current_board_id)
            .execute(&mut *tx)
            .await;

        let _ = sqlx::query("DELETE FROM shapes WHERE board_id = $1")
            .bind(self.current_board_id)
            .execute(&mut *tx)
            .await;

        for shape in self.shapes.values() {
            let color_str = match shape.color {
                Color::Cyan => "Cyan",
                Color::Red => "Red",
                Color::Blue => "Blue",
                Color::White => "White",
                _ => "Gray",
            };

            let res = sqlx::query(
                "INSERT INTO shapes (id, kind, x, y, width, height, label, color, board_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
            )
            .bind(shape.id as i64)
            .bind("Rectangle")
            .bind(shape.rect.x)
            .bind(shape.rect.y)
            .bind(shape.rect.width)
            .bind(shape.rect.height)
            .bind(&shape.label)
            .bind(color_str)
            .bind(self.current_board_id)
            .execute(&mut *tx)
            .await;

            if let Err(e) = res {
                self.status_msg = format!("Save Error: {}", e);
                return;
            }
        }

        for conn in &self.connections {
            let res =
                sqlx::query("INSERT INTO connections (id_a, id_b, board_id) VALUES ($1, $2, $3)")
                    .bind(conn.id_a as i64)
                    .bind(conn.id_b as i64)
                    .bind(self.current_board_id)
                    .execute(&mut *tx)
                    .await;

            if let Err(e) = res {
                self.status_msg = format!("Save Error: {}", e);
                return;
            }
        }

        if let Err(e) = tx.commit().await {
            self.status_msg = format!("Commit Error: {}", e);
        } else {
            self.status_msg = "Saved Successfully!".to_string();
        }
    }

    async fn load_state(&mut self) {
        self.status_msg = "Loading...".to_string();
        self.shapes.clear();
        self.connections.clear();
        self.next_id = 0;

        let rows = match sqlx::query("SELECT * FROM shapes WHERE board_id = $1")
            .bind(self.current_board_id)
            .fetch_all(&self.pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                self.status_msg = format!("Load Error: {}", e);
                return;
            }
        };

        for row in rows {
            let id: i64 = row.get("id");
            let x: f64 = row.get("x");
            let y: f64 = row.get("y");
            let width: f64 = row.get("width");
            let height: f64 = row.get("height");
            let label: String = row.get("label");
            let color_str: String = row.get("color");

            let color = match color_str.as_str() {
                "Cyan" => Color::Cyan,
                "Red" => Color::Red,
                "Blue" => Color::Blue,
                "White" => Color::White,
                _ => Color::Gray,
            };

            let shape = WhiteboardShape {
                id: id as u64,
                kind: ShapeKind::Rectangle,
                rect: canvas::Rectangle {
                    x,
                    y,
                    width,
                    height,
                    color,
                },
                label,
                color,
            };

            if shape.id > self.next_id {
                self.next_id = shape.id;
            }
            self.shapes.insert(shape.id, shape);
        }

        let conn_rows = match sqlx::query("SELECT * FROM connections WHERE board_id = $1")
            .bind(self.current_board_id)
            .fetch_all(&self.pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                self.status_msg = format!("Load Error: {}", e);
                return;
            }
        };

        for row in conn_rows {
            let id_a: i64 = row.get("id_a");
            let id_b: i64 = row.get("id_b");
            self.connections.push(Connection {
                id_a: id_a as u64,
                id_b: id_b as u64,
            });
        }

        if let Ok(row) = sqlx::query("SELECT name FROM boards WHERE id = $1")
            .bind(self.current_board_id)
            .fetch_one(&self.pool)
            .await
        {
            self.current_board_name = row.get("name");
        }

        self.status_msg = format!("Loaded Board: {}", self.current_board_name);
    }

    fn ui(&mut self, frame: &mut Frame) {
        let main_chunks = Layout::vertical([
            Constraint::Length(1), // Toolbar
            Constraint::Min(0),    // Main content
            Constraint::Length(1), // Status Bar
        ])
        .split(frame.area());

        let content_chunks = Layout::horizontal([
            Constraint::Percentage(75), // Canvas
            Constraint::Percentage(25), // Inspector
        ])
        .split(main_chunks[1]);

        self.canvas_area = content_chunks[0];

        // --- 1. Toolbar ---
        // ‼️ UPDATED: Toolbar reflects keyboard controls
        let toolbar_spans = Line::from(vec![
            Span::raw(" (N)ew Rect | (Space) Select | "),
            Span::styled(
                " (L)ink ",
                if self.active_tool == Tool::Connect {
                    Style::new().bg(Color::Blue)
                } else {
                    Style::default()
                },
            ),
            Span::raw(
                " | (Shift+Arrows) Move Shape | (Ctrl+Arrows) Resize | (I)nspect | (S)ave | ",
            ),
            Span::styled(
                " (B)oards ",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw("| (Q)uit"),
        ]);
        frame.render_widget(Paragraph::new(toolbar_spans), main_chunks[0]);

        // --- 2. Canvas ---
        // ‼️ Check camera bounds before drawing
        self.update_camera();

        let x_bounds = [self.pan_offset.0, self.pan_offset.0 + self.view_size.0];
        let y_bounds = [self.pan_offset.1, self.pan_offset.1 + self.view_size.1];

        let canvas = Canvas::default()
            .block(
                Block::default()
                    .title(format!("Whiteboard: {}", self.current_board_name))
                    .borders(Borders::ALL),
            )
            .x_bounds(x_bounds)
            .y_bounds(y_bounds)
            .paint(|ctx| {
                self.draw_on_canvas(ctx);
            });
        frame.render_widget(canvas, self.canvas_area);

        // --- 3. Inspector Panel ---
        self.draw_inspector(frame, content_chunks[1]);

        // --- 4. Status Bar ---
        self.draw_status_bar(frame, main_chunks[2]);

        if self.mode == Mode::BoardMenu {
            self.draw_board_menu(frame);
        }
    }

    // ‼️ NEW: Helper to keep the cursor inside the visible view
    fn update_camera(&mut self) {
        let margin = 10.0;
        // Check Right
        if self.cursor_pos.0 > (self.pan_offset.0 + self.view_size.0 - margin) {
            self.pan_offset.0 = self.cursor_pos.0 - self.view_size.0 + margin;
        }
        // Check Left
        if self.cursor_pos.0 < (self.pan_offset.0 + margin) {
            self.pan_offset.0 = self.cursor_pos.0 - margin;
        }
        // Check Top
        if self.cursor_pos.1 > (self.pan_offset.1 + self.view_size.1 - margin) {
            self.pan_offset.1 = self.cursor_pos.1 - self.view_size.1 + margin;
        }
        // Check Bottom
        if self.cursor_pos.1 < (self.pan_offset.1 + margin) {
            self.pan_offset.1 = self.cursor_pos.1 - margin;
        }
    }

    fn draw_board_menu(&mut self, frame: &mut Frame) {
        let area = centered_rect(60, 50, frame.area());
        frame.render_widget(Clear, area);
        let outer_block = Block::default()
            .title(" Select Board ")
            .borders(Borders::ALL)
            .style(Style::default().bg(Color::DarkGray));
        frame.render_widget(outer_block.clone(), area);

        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)])
            .margin(1)
            .split(area);

        let items: Vec<ListItem> = self
            .available_boards
            .iter()
            .map(|b| {
                let style = if b.id == self.current_board_id {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(format!("{} (ID: {})", b.name, b.id)).style(style)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::BOTTOM).title("Available"))
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, chunks[0], &mut self.board_list_state);

        let input_text = vec![
            Line::from("Type name & Press 'Ctrl+N' to create new."),
            Line::from(Span::styled(
                format!("New Name: {}", self.new_board_input),
                Style::default().fg(Color::Yellow),
            )),
        ];
        let input = Paragraph::new(input_text).block(Block::default().borders(Borders::NONE));
        frame.render_widget(input, chunks[1]);
    }

    fn draw_inspector(&self, frame: &mut Frame, area: Rect) {
        let mut text = Vec::new();
        if let Some(selected_id) = self.selected_shape_id {
            if let Some(shape) = self.shapes.get(&selected_id) {
                text.push(Line::from(Span::styled(
                    "Inspector",
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                text.push(Line::from(format!("ID: {}", shape.id)));
                text.push(Line::from(format!(
                    "Kind: {}",
                    match shape.kind {
                        ShapeKind::Rectangle => "Rectangle",
                    }
                )));
                text.push(Line::from("Label:"));
                if self.mode == Mode::Editing {
                    text.push(Line::from(Span::styled(
                        format!("> {}_", self.label_edit_buffer),
                        Style::new().bg(Color::White).fg(Color::Black),
                    )));
                } else {
                    text.push(Line::from(format!("> {}", shape.label)));
                    text.push(Line::from("(Press 'i' to edit)"));
                }
                text.push(Line::from(""));
                text.push(Line::from("Dims:"));
                text.push(Line::from(format!(
                    "W: {:.1} H: {:.1}",
                    shape.rect.width, shape.rect.height
                )));
                text.push(Line::from(""));
                text.push(Line::from("(Del) to delete"));
            }
        } else {
            text.push(Line::from("Select a shape to inspect it."));
        }
        frame.render_widget(
            Paragraph::new(text).block(Block::default().title("Inspector").borders(Borders::ALL)),
            area,
        );
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        let mode_str = match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Editing => "EDITING",
            Mode::BoardMenu => "BOARDS",
        };
        // ‼️ UPDATED: Status bar shows Cursor Position
        let status_spans = Line::from(vec![
            Span::styled(format!(" {} ", mode_str), Style::new().bg(Color::Red)),
            Span::raw(format!(
                " | Cursor: ({:.1}, {:.1}) | MSG: {}",
                self.cursor_pos.0, self.cursor_pos.1, self.status_msg
            )),
        ]);
        frame.render_widget(Paragraph::new(status_spans), area);
    }

    fn draw_on_canvas(&self, ctx: &mut Context) {
        // --- Draw Connections ---
        for conn in &self.connections {
            if let (Some(a), Some(b)) = (self.shapes.get(&conn.id_a), self.shapes.get(&conn.id_b)) {
                let center_a = a.center();
                let center_b = b.center();
                let (x1, y1) = a.get_boundary_point(center_b);
                let (x2, y2) = b.get_boundary_point(center_a);
                ctx.draw(&CanvasLine {
                    x1,
                    y1,
                    x2,
                    y2,
                    color: Color::DarkGray,
                });
            }
        }

        // --- Draw Shapes ---
        for (id, shape) in &self.shapes {
            let mut color = shape.color;
            let is_selected = self.selected_shape_id == Some(*id);
            if is_selected {
                color = Color::Blue;
            }
            if self.connect_start_id == Some(*id) {
                color = Color::Red;
            }

            // Draw the shape
            match shape.kind {
                ShapeKind::Rectangle => {
                    ctx.draw(&Rectangle {
                        color,
                        ..shape.rect
                    });
                }
            }

            // Draw the label
            if !shape.label.is_empty() {
                let x_offset = shape.label.len() as f64 / 2.0;
                ctx.print(
                    shape.center().0 - x_offset,
                    shape.center().1,
                    Line::from(shape.label.clone()).fg(Color::White),
                );
            }
        }

        // ‼️ UPDATED: Draw Virtual Cursor (Crosshair)
        let cx = self.cursor_pos.0;
        let cy = self.cursor_pos.1;
        let cursor_size = 2.0;

        ctx.draw(&CanvasLine {
            x1: cx - cursor_size,
            y1: cy,
            x2: cx + cursor_size,
            y2: cy,
            color: Color::Yellow,
        });
        ctx.draw(&CanvasLine {
            x1: cx,
            y1: cy - cursor_size,
            x2: cx,
            y2: cy + cursor_size,
            color: Color::Yellow,
        });
    }

    // ‼️ MAJOR REFACTOR: Handle key events for cursor/selection/modification
    async fn handle_key_event(&mut self, key: event::KeyEvent) {
        if self.mode == Mode::Editing {
            match key.code {
                KeyCode::Enter => {
                    if let Some(id) = self.selected_shape_id {
                        if let Some(shape) = self.shapes.get_mut(&id) {
                            shape.label = self.label_edit_buffer.clone();
                        }
                    }
                    self.mode = Mode::Normal;
                }
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Char(c) => {
                    self.label_edit_buffer.push(c);
                }
                KeyCode::Backspace => {
                    self.label_edit_buffer.pop();
                }
                _ => {}
            }
        } else if self.mode == Mode::BoardMenu {
            match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(i) = self.board_list_state.selected() {
                        if i < self.available_boards.len().saturating_sub(1) {
                            self.board_list_state.select(Some(i + 1));
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(i) = self.board_list_state.selected() {
                        if i > 0 {
                            self.board_list_state.select(Some(i - 1));
                        }
                    }
                }
                KeyCode::Enter => {
                    if let Some(i) = self.board_list_state.selected() {
                        if let Some(board) = self.available_boards.get(i) {
                            let board_id = board.id;
                            self.current_board_id = board_id;
                            self.current_board_name = board.name.clone();
                            self.load_state().await;
                            self.mode = Mode::Normal;
                        }
                    }
                }
                KeyCode::Char('n') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    let name = self.new_board_input.clone();
                    if !name.is_empty() {
                        self.create_board(&name).await;
                        self.new_board_input.clear();
                    }
                }
                KeyCode::Char(c) => {
                    self.new_board_input.push(c);
                }
                KeyCode::Backspace => {
                    self.new_board_input.pop();
                }
                _ => {}
            }
        } else {
            // --- Normal Mode Input ---
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,

                // ‼️ MOVEMENT (Arrows / HJKL)
                KeyCode::Left | KeyCode::Char('h') => self.move_left(key.modifiers),
                KeyCode::Right | KeyCode::Char('l')
                    if key.modifiers.is_empty()
                        || key.modifiers.contains(KeyModifiers::SHIFT)
                        || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.move_right(key.modifiers)
                }
                KeyCode::Up | KeyCode::Char('k') => self.move_up(key.modifiers),
                KeyCode::Down | KeyCode::Char('j') => self.move_down(key.modifiers),

                // ‼️ SELECTION / ACTION
                KeyCode::Char(' ') | KeyCode::Enter => {
                    // Try to find shape under cursor
                    if let Some(id) = self.get_shape_at(self.cursor_pos.0, self.cursor_pos.1) {
                        self.selected_shape_id = Some(id);
                        self.status_msg = format!("Selected Shape {}", id);

                        // Link logic
                        if self.active_tool == Tool::Connect {
                            if let Some(start) = self.connect_start_id {
                                if start != id {
                                    self.connections.push(Connection {
                                        id_a: start,
                                        id_b: id,
                                    });
                                    self.connect_start_id = None;
                                    self.active_tool = Tool::Pointer;
                                    self.status_msg = "Connected!".to_string();
                                }
                            } else {
                                self.connect_start_id = Some(id);
                                self.status_msg = "Link Start... Select target.".to_string();
                            }
                        }
                    } else {
                        self.selected_shape_id = None;
                        self.connect_start_id = None;
                        self.status_msg = "Cleared Selection".to_string();
                    }
                }

                // ‼️ NEW SHAPE
                KeyCode::Char('n') | KeyCode::Char('N')
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    let id = self.new_id();
                    let new_shape = WhiteboardShape {
                        id,
                        kind: ShapeKind::Rectangle,
                        rect: canvas::Rectangle {
                            x: self.cursor_pos.0 - 5.0,
                            y: self.cursor_pos.1 - 2.5,
                            width: 10.0,
                            height: 5.0,
                            color: Color::Cyan,
                        },
                        label: String::new(),
                        color: Color::Cyan,
                    };
                    self.shapes.insert(id, new_shape);
                    self.selected_shape_id = Some(id);
                    self.status_msg = "Created Rectangle".to_string();
                }

                // ‼️ LINK MODE
                KeyCode::Char('L') => {
                    self.active_tool = Tool::Connect;
                    self.status_msg = "Link Mode: Select Start Shape".to_string();
                }

                // General
                KeyCode::Char('s') | KeyCode::Char('S') => self.save_state().await,
                KeyCode::Char('o') | KeyCode::Char('O') => self.load_state().await,
                KeyCode::Char('b') | KeyCode::Char('B') => {
                    self.refresh_board_list().await;
                    self.mode = Mode::BoardMenu;
                }
                KeyCode::Char('i') | KeyCode::Char('I') => {
                    if self.selected_shape_id.is_some() {
                        self.mode = Mode::Editing;
                        self.label_edit_buffer = self
                            .shapes
                            .get(&self.selected_shape_id.unwrap())
                            .map_or(String::new(), |s| s.label.clone());
                    }
                }
                KeyCode::Delete | KeyCode::Char('x') => {
                    if let Some(id) = self.selected_shape_id.take() {
                        self.shapes.remove(&id);
                        self.connections.retain(|c| c.id_a != id && c.id_b != id);
                        self.status_msg = "Deleted Shape".to_string();
                    }
                }
                KeyCode::Esc => {
                    self.selected_shape_id = None;
                    self.connect_start_id = None;
                    self.active_tool = Tool::Pointer;
                }
                _ => {}
            }
        }
    }

    // ‼️ MOVEMENT HELPERS
    // Shift = Move Shape, Ctrl = Resize Shape, None = Move Cursor
    fn move_left(&mut self, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::SHIFT) && self.selected_shape_id.is_some() {
            if let Some(s) = self.shapes.get_mut(&self.selected_shape_id.unwrap()) {
                s.rect.x -= self.move_speed;
            }
            self.cursor_pos.0 -= self.move_speed;
        } else if mods.contains(KeyModifiers::CONTROL) && self.selected_shape_id.is_some() {
            if let Some(s) = self.shapes.get_mut(&self.selected_shape_id.unwrap()) {
                s.rect.width = (s.rect.width - self.move_speed).max(1.0);
            }
        } else {
            self.cursor_pos.0 -= self.move_speed;
        }
    }

    fn move_right(&mut self, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::SHIFT) && self.selected_shape_id.is_some() {
            if let Some(s) = self.shapes.get_mut(&self.selected_shape_id.unwrap()) {
                s.rect.x += self.move_speed;
            }
            self.cursor_pos.0 += self.move_speed;
        } else if mods.contains(KeyModifiers::CONTROL) && self.selected_shape_id.is_some() {
            if let Some(s) = self.shapes.get_mut(&self.selected_shape_id.unwrap()) {
                s.rect.width += self.move_speed;
            }
        } else {
            self.cursor_pos.0 += self.move_speed;
        }
    }

    fn move_up(&mut self, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::SHIFT) && self.selected_shape_id.is_some() {
            if let Some(s) = self.shapes.get_mut(&self.selected_shape_id.unwrap()) {
                s.rect.y += self.move_speed;
            }
            self.cursor_pos.1 += self.move_speed;
        } else if mods.contains(KeyModifiers::CONTROL) && self.selected_shape_id.is_some() {
            if let Some(s) = self.shapes.get_mut(&self.selected_shape_id.unwrap()) {
                s.rect.height += self.move_speed;
            }
        } else {
            self.cursor_pos.1 += self.move_speed;
        }
    }

    fn move_down(&mut self, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::SHIFT) && self.selected_shape_id.is_some() {
            if let Some(s) = self.shapes.get_mut(&self.selected_shape_id.unwrap()) {
                s.rect.y -= self.move_speed;
            }
            self.cursor_pos.1 -= self.move_speed;
        } else if mods.contains(KeyModifiers::CONTROL) && self.selected_shape_id.is_some() {
            if let Some(s) = self.shapes.get_mut(&self.selected_shape_id.unwrap()) {
                s.rect.height = (s.rect.height - self.move_speed).max(1.0);
            }
        } else {
            self.cursor_pos.1 -= self.move_speed;
        }
    }

    fn on_resize(&mut self, _width: u16, _height: u16) {
        // Layout recalculated on next draw
    }

    fn on_tick(&mut self) {
        // Future animation logic
    }

    /// Finds the top-most shape at a given world coordinate
    fn get_shape_at(&self, x: f64, y: f64) -> Option<u64> {
        self.shapes
            .iter()
            .filter(|(_, shape)| shape.contains(x, y))
            .map(|(id, _)| *id)
            .last()
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

// --- TUI Boilerplate ---
struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    pub fn new() -> io::Result<Self> {
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        // ‼️ Removed EnableMouseCapture
        terminal.clear()?;
        Ok(Self { terminal })
    }

    pub fn draw<F>(&mut self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.terminal.draw(f)?;
        Ok(())
    }

    pub fn exit(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        io::stdout().execute(LeaveAlternateScreen)?;
        // ‼️ Removed DisableMouseCapture
        Ok(())
    }
}

