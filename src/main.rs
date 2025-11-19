use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, MouseEvent,
        MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use futures_util::StreamExt;
use ratatui::{
    prelude::*,
    widgets::{
        canvas::{self, Canvas, Context, Line as CanvasLine, Rectangle},
        Block, Borders, Paragraph,
    },
};
use std::{
    collections::HashMap,
    io::{self, Stdout},
    time::{Duration, Instant},
};

/// We need tokio's runtime for the async event loop
#[tokio::main]
async fn main() -> io::Result<()> {
    // --- TUI Setup ---
    let mut terminal = Tui::new()?;

    // --- App State ---
    let mut app = App::default();
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
                Some(Ok(Event::Key(key))) => app.handle_key_event(key),
                Some(Ok(Event::Mouse(mouse))) => app.handle_mouse_event(mouse),
                Some(Ok(Event::Resize(width, height))) => app.on_resize(width, height),
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
    DrawRect,
    Connect,
}

#[derive(PartialEq, Eq, Clone, Copy, Default)]
enum Mode {
    #[default]
    Normal,
    Editing,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum ResizeHandle {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

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

        // If centers are identical, just return center
        if dx == 0.0 && dy == 0.0 {
            return (cx, cy);
        }

        let half_w = self.rect.width / 2.0;
        let half_h = self.rect.height / 2.0;

        // Determine how much we need to scale the vector (dx, dy) to hit the nearest edge.
        // We want the smallest scale factor that hits a boundary (width or height).
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

    /// Moves the shape by a delta
    fn translate(&mut self, dx: f64, dy: f64) {
        self.rect.x += dx;
        self.rect.y += dy;
    }

    fn resize(&mut self, handle: ResizeHandle, target_x: f64, target_y: f64) {
        match handle {
            ResizeHandle::TopRight => {
                // x unchanged, y unchanged (bottom-left anchor), width/height changes
                // Visually Top-Right is World (x+w, y+h)
                let new_width = (target_x - self.rect.x).max(1.0);
                let new_height = (target_y - self.rect.y).max(1.0);
                self.rect.width = new_width;
                self.rect.height = new_height;
            }
            ResizeHandle::BottomRight => {
                // Visually Bottom-Right is World (x+w, y)
                // x unchanged, top (y+h) anchor unchanged.
                // Wait, y is bottom. dragging bottom right changes width and y.
                let new_width = (target_x - self.rect.x).max(1.0);
                let old_top = self.rect.y + self.rect.height;
                let new_height = (old_top - target_y).max(1.0);

                self.rect.width = new_width;
                self.rect.y = target_y; // moving bottom edge
                self.rect.height = new_height;
            }
            ResizeHandle::BottomLeft => {
                // Visually Bottom-Left is World (x, y)
                // Changing x and y (both min values)
                let old_right = self.rect.x + self.rect.width;
                let old_top = self.rect.y + self.rect.height;

                let new_width = (old_right - target_x).max(1.0);
                let new_height = (old_top - target_y).max(1.0);

                self.rect.x = target_x;
                self.rect.y = target_y;
                self.rect.width = new_width;
                self.rect.height = new_height;
            }
            ResizeHandle::TopLeft => {
                // Visually Top-Left is World (x, y+h)
                // Changing x (left) and height (top). y (bottom) is anchor.
                let old_right = self.rect.x + self.rect.width;

                let new_width = (old_right - target_x).max(1.0);
                let new_height = (target_y - self.rect.y).max(1.0);

                self.rect.x = target_x;
                self.rect.width = new_width;
                self.rect.height = new_height;
            }
        }
    }

    /// Checks if a world coordinate (x, y) is inside this shape
    fn contains(&self, x: f64, y: f64) -> bool {
        match self.kind {
            ShapeKind::Rectangle => {
                (x >= self.rect.x && x <= (self.rect.x + self.rect.width))
                    && (y >= self.rect.y && y <= (self.rect.y + self.rect.height))
            }
        }
    }

    fn get_handle_collision(&self, x: f64, y: f64, threshold: f64) -> Option<ResizeHandle> {
        // World Coords: Y is UP.
        // BL = (x, y)
        // BR = (x+w, y)
        // TL = (x, y+h)
        // TR = (x+w, y+h)
        let left = self.rect.x;
        let right = self.rect.x + self.rect.width;
        let bottom = self.rect.y;
        let top = self.rect.y + self.rect.height;

        // Helper to check distance

        let is_near = |px: f64, py: f64| (x - px).abs() < threshold && (y - py).abs() < threshold;

        if is_near(left, top) {
            return Some(ResizeHandle::TopLeft);
        }
        if is_near(right, top) {
            return Some(ResizeHandle::TopRight);
        }
        if is_near(left, bottom) {
            return Some(ResizeHandle::BottomLeft);
        }
        if is_near(right, bottom) {
            return Some(ResizeHandle::BottomRight);
        }
        None
    }
}
/// Represents a connection between two shapes, identified by their IDs.
struct Connection {
    id_a: u64,
    id_b: u64,
}

/// Holds the entire state of our application.
struct App {
    shapes: HashMap<u64, WhiteboardShape>,
    connections: Vec<Connection>,
    active_tool: Tool,
    mode: Mode,

    /// ID of the shape currently being dragged.
    dragged_shape_id: Option<u64>,
    /// ID of the currently selected shape.
    selected_shape_id: Option<u64>,

    resizing_handle: Option<ResizeHandle>,
    is_resizing: bool,

    label_edit_buffer: String,
    connect_start_id: Option<u64>,

    next_id: u64,

    /// Pan offset (top-left corner of the view in world coords)
    pan_offset: (f64, f64),
    /// View dimensions in world coords
    view_size: (f64, f64),

    /// The Rect of the terminal area allocated to the canvas
    canvas_area: Rect,

    /// The last known mouse position (in terminal cells)
    mouse_cursor_pos: (u16, u16),
    /// Start position of a pan (in terminal cells)
    pan_start_pos: Option<(u16, u16)>,
    /// Start position of a drag (in world coords)
    drag_start_pos: Option<(f64, f64)>,

    is_panning: bool,
    is_dragging: bool,
    should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            shapes: HashMap::new(),
            connections: Vec::new(),
            active_tool: Tool::Pointer,
            mode: Mode::Normal,
            dragged_shape_id: None,
            selected_shape_id: None,
            resizing_handle: None,
            is_resizing: false,
            label_edit_buffer: String::new(),
            connect_start_id: None,
            next_id: 0,
            pan_offset: (0.0, 0.0),
            view_size: (200.0, 100.0), // Default 200x100 world units
            canvas_area: Rect::default(),
            mouse_cursor_pos: (0, 0),
            pan_start_pos: None,
            drag_start_pos: None,
            is_panning: false,
            is_dragging: false,
            should_quit: false,
        }
    }
}

impl App {
    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Main draw call
    fn ui(&mut self, frame: &mut Frame) {
        // --- Layout ---
        // Total layout: [Toolbar] [Main Area] [Status Bar]
        let main_chunks = Layout::vertical([
            Constraint::Length(1), // Toolbar
            Constraint::Min(0),    // Main content
            Constraint::Length(1), // Status Bar
        ])
        .split(frame.size());

        // Split main content: [Canvas] [Inspector]
        let content_chunks = Layout::horizontal([
            Constraint::Percentage(75), // Canvas
            Constraint::Percentage(25), // Inspector
        ])
        .split(main_chunks[1]);

        self.canvas_area = content_chunks[0]; // Store canvas area for coord conversion

        // --- 1. Toolbar ---
        let toolbar_spans = Line::from(vec![
            Span::styled(
                " (P)ointer ",
                if self.active_tool == Tool::Pointer {
                    Style::new().bg(Color::Blue)
                } else {
                    Style::default()
                },
            ),
            Span::styled(
                " (R)ect ",
                if self.active_tool == Tool::DrawRect {
                    Style::new().bg(Color::Blue)
                } else {
                    Style::default()
                },
            ),
            Span::styled(
                " (L)ink ",
                if self.active_tool == Tool::Connect {
                    Style::new().bg(Color::Blue)
                } else {
                    Style::default()
                },
            ),
            Span::raw(" | (I)nspect/Edit | (Q)uit"),
        ]);
        frame.render_widget(Paragraph::new(toolbar_spans), main_chunks[0]);

        // --- 2. Canvas ---
        let x_bounds = [self.pan_offset.0, self.pan_offset.0 + self.view_size.0];
        let y_bounds = [self.pan_offset.1, self.pan_offset.1 + self.view_size.1];

        let canvas = Canvas::default()
            .block(Block::default().title("Whiteboard").borders(Borders::ALL))
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
    }

    /// Draws the content of the inspector panel
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
                    // Show text buffer with a "cursor"
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

    /// Draws the bottom status bar
    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        let (world_x, world_y) =
            self.terminal_to_world_coords(self.mouse_cursor_pos.0, self.mouse_cursor_pos.1);
        let mode_str = match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Editing => "EDITING",
        };
        let status_spans = Line::from(vec![
            Span::styled(format!(" {} ", mode_str), Style::new().bg(Color::Red)),
            Span::raw(format!(
                " | Mouse: ({}, {}) | World: ({:.1}, {:.1})",
                self.mouse_cursor_pos.0, self.mouse_cursor_pos.1, world_x, world_y
            )),
        ]);
        frame.render_widget(Paragraph::new(status_spans), area);
    }

    /// Main logic for drawing shapes/lines on the canvas
    fn draw_on_canvas(&self, ctx: &mut Context) {
        // --- Draw Connections ---
        for conn in &self.connections {
            if let (Some(a), Some(b)) = (self.shapes.get(&conn.id_a), self.shapes.get(&conn.id_b)) {
                // ‼️ CHANGED: Calculate intersection points on the borders
                let center_a = a.center();
                let center_b = b.center();

                // Find point on A's border looking at B
                let (x1, y1) = a.get_boundary_point(center_b);
                // Find point on B's border looking at A
                let (x2, y2) = b.get_boundary_point(center_a);

                ctx.draw(&CanvasLine {
                    x1, // ‼️ was: a.center().0
                    y1, // ‼️ was: a.center().1
                    x2, // ‼️ was: b.center().0
                    y2, // ‼️ was: b.center().1
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

            if is_selected {
                // Draw small squares at corners
                let handle_size = 2.0; // 2 world units wide
                let half = handle_size / 2.0;

                // BL, BR, TL, TR
                let corners = vec![
                    (shape.rect.x, shape.rect.y),                     // BL
                    (shape.rect.x + shape.rect.width, shape.rect.y),  // BR
                    (shape.rect.x, shape.rect.y + shape.rect.height), // TL
                    (
                        shape.rect.x + shape.rect.width,
                        shape.rect.y + shape.rect.height,
                    ), // TR
                ];

                for (cx, cy) in corners {
                    ctx.draw(&Rectangle {
                        x: cx - half,
                        y: cy - half,
                        width: handle_size,
                        height: handle_size,
                        color: Color::White,
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

        // --- Draw Cursor ---
        // Convert mouse cell pos to world coords
        let (world_x, world_y) =
            self.terminal_to_world_coords(self.mouse_cursor_pos.0, self.mouse_cursor_pos.1);

        // Draw a small crosshair
        ctx.draw(&CanvasLine {
            x1: world_x - 1.0,
            y1: world_y,
            x2: world_x + 1.0,
            y2: world_y,
            color: Color::Yellow,
        });
        ctx.draw(&CanvasLine {
            x1: world_x,
            y1: world_y - 0.5,
            x2: world_x,
            y2: world_y + 0.5,
            color: Color::Yellow,
        });
    }

    /// Handle key presses
    fn handle_key_event(&mut self, key: event::KeyEvent) {
        if self.mode == Mode::Editing {
            // --- Editing Mode Input ---
            match key.code {
                KeyCode::Enter => {
                    // Save buffer to shape and exit editing mode
                    if let Some(id) = self.selected_shape_id {
                        if let Some(shape) = self.shapes.get_mut(&id) {
                            shape.label = self.label_edit_buffer.clone();
                        }
                    }
                    self.mode = Mode::Normal;
                }
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    // Discard changes by not saving buffer
                }
                KeyCode::Char(c) => {
                    self.label_edit_buffer.push(c);
                }
                KeyCode::Backspace => {
                    self.label_edit_buffer.pop();
                }
                _ => {}
            }
        } else {
            // --- Normal Mode Input ---
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
                KeyCode::Char('p') | KeyCode::Char('P') => self.active_tool = Tool::Pointer,
                KeyCode::Char('r') | KeyCode::Char('R') => self.active_tool = Tool::DrawRect,
                KeyCode::Char('l') | KeyCode::Char('L') => self.active_tool = Tool::Connect,
                KeyCode::Char('i') | KeyCode::Char('I') => {
                    if self.selected_shape_id.is_some() {
                        self.mode = Mode::Editing;
                        // Load label into buffer
                        self.label_edit_buffer = self
                            .shapes
                            .get(&self.selected_shape_id.unwrap())
                            .map_or(String::new(), |s| s.label.clone());
                    }
                }
                KeyCode::Delete => {
                    if let Some(id) = self.selected_shape_id.take() {
                        self.shapes.remove(&id);
                        self.connections.retain(|c| c.id_a != id && c.id_b != id);
                    }
                }
                KeyCode::Esc => {
                    self.selected_shape_id = None;
                    self.connect_start_id = None;
                }
                _ => {}
            }
        }
    }

    /// Handle mouse events
    fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        // Update mouse position
        self.mouse_cursor_pos = (mouse.column, mouse.row);

        // Check if mouse is within the canvas area
        if !self
            .canvas_area
            .contains(Position::new(mouse.column, mouse.row))
        {
            // Mouse is outside canvas, don't process clicks/drags
            // We still update position for the status bar, though.
            return;
        }

        // Convert to world coordinates
        let (world_x, world_y) = self.terminal_to_world_coords(mouse.column, mouse.row);

        match mouse.kind {
            // --- Mouse Press ---
            MouseEventKind::Down(button) => {
                let hovered_id = self.get_shape_at(world_x, world_y);

                match button {
                    event::MouseButton::Left => match self.active_tool {
                        Tool::Pointer => {
                            let mut handle_hit = None;
                            if let Some(sel_id) = self.selected_shape_id {
                                if let Some(shape) = self.shapes.get(&sel_id) {
                                    // Tolerance: 2.0 world units
                                    if let Some(handle) =
                                        shape.get_handle_collision(world_x, world_y, 2.0)
                                    {
                                        handle_hit = Some(handle);
                                    }
                                }
                            }

                            if let Some(handle) = handle_hit {
                                // We clicked a resize handle!
                                self.is_resizing = true;
                                self.resizing_handle = Some(handle);
                                // Keep selected_shape_id as is
                            } else {
                                // Normal selection logic
                                self.selected_shape_id = hovered_id;
                                if let Some(id) = hovered_id {
                                    self.is_dragging = true;
                                    self.dragged_shape_id = Some(id);
                                    self.drag_start_pos = Some((world_x, world_y));
                                }
                            }
                        }
                        Tool::DrawRect => {
                            let id = self.new_id();
                            let new_shape = WhiteboardShape {
                                id,
                                kind: ShapeKind::Rectangle,
                                rect: canvas::Rectangle {
                                    x: world_x - 5.0,
                                    y: world_y - 2.5,
                                    width: 10.0,
                                    height: 5.0,
                                    color: Color::Cyan,
                                },
                                label: String::new(),
                                color: Color::Cyan,
                            };
                            self.shapes.insert(id, new_shape);
                        }
                        Tool::Connect => {
                            if let Some(id) = hovered_id {
                                if let Some(start_id) = self.connect_start_id.take() {
                                    self.connections.push(Connection {
                                        id_a: start_id,
                                        id_b: id,
                                    });
                                } else {
                                    self.connect_start_id = Some(id);
                                }
                            }
                        }
                    },
                    event::MouseButton::Middle => {
                        self.is_panning = true;
                        self.pan_start_pos = Some((mouse.column, mouse.row));
                    }
                    _ => {}
                }
            }

            // --- Mouse Drag ---
            MouseEventKind::Drag(button) => {
                match button {
                    event::MouseButton::Left => {
                        if self.is_resizing {
                            if let (Some(id), Some(handle)) =
                                (self.selected_shape_id, self.resizing_handle)
                            {
                                if let Some(shape) = self.shapes.get_mut(&id) {
                                    shape.resize(handle, world_x, world_y);
                                }
                            }
                        } else if self.is_dragging {
                            if let (Some(id), Some(start_pos)) =
                                (self.dragged_shape_id, self.drag_start_pos)
                            {
                                if let Some(shape) = self.shapes.get_mut(&id) {
                                    let dx = world_x - start_pos.0;
                                    let dy = world_y - start_pos.1;
                                    shape.translate(dx, dy);
                                    self.drag_start_pos = Some((world_x, world_y));
                                }
                            }
                        }
                    }
                    event::MouseButton::Middle => {
                        if self.is_panning {
                            if let Some(start_pos) = self.pan_start_pos {
                                // Delta in terminal cells
                                let dx_term = mouse.column as f64 - start_pos.0 as f64;
                                let dy_term = mouse.row as f64 - start_pos.1 as f64;

                                // Convert to world delta
                                let dx_world =
                                    (dx_term / self.canvas_area.width as f64) * self.view_size.0;
                                let dy_world =
                                    (dy_term / self.canvas_area.height as f64) * self.view_size.1;

                                // Pan by subtracting delta (inverted Y)
                                self.pan_offset.0 -= dx_world;
                                self.pan_offset.1 += dy_world; // Y is inverted in loop but consistent here
                                                               // Reset start pos for next drag event
                                self.pan_start_pos = Some((mouse.column, mouse.row));
                            }
                        }
                    }
                    _ => {}
                }
            }

            // --- Mouse Release ---
            MouseEventKind::Up(button) => match button {
                event::MouseButton::Left => {
                    self.is_dragging = false;
                    self.dragged_shape_id = None;
                    self.drag_start_pos = None;

                    self.is_resizing = false;
                    self.resizing_handle = None;
                }
                event::MouseButton::Middle => {
                    self.is_panning = false;
                    self.pan_start_pos = None;
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Called on terminal resize
    fn on_resize(&mut self, _width: u16, _height: u16) {
        // The layout will be recalculated on the next draw,
        // which will update self.canvas_area.
    }

    /// Called on a regular interval
    fn on_tick(&mut self) {
        // Future use: animations, etc.
    }

    fn terminal_to_world_coords(&self, col: u16, row: u16) -> (f64, f64) {
        if self.canvas_area.width == 0 || self.canvas_area.height == 0 {
            return (0.0, 0.0);
        }

        // The drawing area is 1 cell inward from the layout chunk
        let inner_x = self.canvas_area.x + 1;
        let inner_y = self.canvas_area.y + 1;
        let inner_width = self.canvas_area.width.saturating_sub(2).max(1);
        let inner_height = self.canvas_area.height.saturating_sub(2).max(1);

        // Normalize coordinates within the canvas area (0.0 to 1.0)
        let norm_x = (col.saturating_sub(inner_x)) as f64 / inner_width as f64;
        let norm_y = (row.saturating_sub(inner_y)) as f64 / inner_height as f64;

        // Map to world coordinates
        // Y is inverted: 0.0 at top of terminal, 1.0 at bottom
        let world_x = self.pan_offset.0 + norm_x * self.view_size.0;
        let world_y = self.pan_offset.1 + (1.0 - norm_y) * self.view_size.1;

        (world_x, world_y)
    }

    /// Finds the top-most shape at a given world coordinate
    fn get_shape_at(&self, x: f64, y: f64) -> Option<u64> {
        // Iterate in reverse to get the "top-most" (last drawn)
        self.shapes
            .iter()
            .filter(|(_, shape)| shape.contains(x, y))
            .map(|(id, _)| *id)
            .last()
    }
}

// --- TUI Boilerplate ---

/// A simple wrapper for terminal setup and teardown
struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    /// Create a new TUI
    pub fn new() -> io::Result<Self> {
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;

        // Setup terminal
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        io::stdout().execute(EnableMouseCapture)?;

        // Clear screen
        terminal.clear()?;

        Ok(Self { terminal })
    }

    /// Draw a frame
    pub fn draw<F>(&mut self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.terminal.draw(f)?;
        Ok(())
    }

    /// Restore terminal on exit
    pub fn exit(&mut self) -> io::Result<()> {
        // Restore terminal
        disable_raw_mode()?;
        io::stdout().execute(LeaveAlternateScreen)?;
        io::stdout().execute(DisableMouseCapture)?;
        Ok(())
    }
}
