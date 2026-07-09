use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use slipstream::{Command, Engine, EventLog, Side, protocol};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(command) = std::env::args().nth(1) else {
        print_usage();
        return Ok(());
    };

    match command.as_str() {
        "demo" => run_demo()?,
        "serve" => run_server()?,
        "web" => run_web()?,
        "submit" => run_submit()?,
        "recover" => run_recover()?,
        _ => print_usage(),
    }

    Ok(())
}

fn run_web() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:8080".to_owned());
    let log_path = std::env::args()
        .nth(3)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/slipstream-web.events"));

    let (engine, _) = if log_path.exists() {
        EventLog::recover(&log_path)?
    } else {
        (Engine::new(), Vec::new())
    };
    let engine = Arc::new(Mutex::new(engine));
    let listener = TcpListener::bind(&addr)?;

    println!("Slipstream web UI listening on http://{addr}");
    println!("event log: {}", log_path.display());

    for stream in listener.incoming() {
        let stream = stream?;
        let engine = Arc::clone(&engine);
        let log_path = log_path.clone();
        thread::spawn(move || {
            if let Err(err) = handle_http(stream, engine, log_path) {
                eprintln!("web client error: {err}");
            }
        });
    }

    Ok(())
}

fn handle_http(
    mut stream: TcpStream,
    engine: Arc<Mutex<Engine>>,
    log_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }

    let mut content_length = 0_usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }

        if let Some(value) = header.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();

    match (method, path) {
        ("GET", "/") => write_http(&mut stream, "200 OK", "text/html; charset=utf-8", WEB_APP),
        ("GET", "/api/snapshot") => {
            let snapshot = engine.lock().expect("engine mutex poisoned").snapshot();
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                &snapshot_json(&snapshot),
            )
        }
        ("POST", "/api/order") => {
            let response = submit_web_command(body.trim(), engine, log_path);
            write_http(
                &mut stream,
                response.status,
                "application/json",
                &response.body,
            )
        }
        _ => write_http(
            &mut stream,
            "404 Not Found",
            "application/json",
            r#"{"ok":false,"message":"not found"}"#,
        ),
    }?;

    Ok(())
}

struct WebResponse {
    status: &'static str,
    body: String,
}

fn submit_web_command(command: &str, engine: Arc<Mutex<Engine>>, log_path: PathBuf) -> WebResponse {
    let command = match protocol::parse_command(command) {
        Ok(command) => command,
        Err(err) => {
            return WebResponse {
                status: "400 Bad Request",
                body: format!(
                    r#"{{"ok":false,"message":"{}"}}"#,
                    json_escape(&err.to_string())
                ),
            };
        }
    };

    let mut engine = engine.lock().expect("engine mutex poisoned");
    match engine.submit(command) {
        Ok(accepted) => match EventLog::append(&log_path, &accepted.event) {
            Ok(()) => WebResponse {
                status: "200 OK",
                body: format!(
                    r#"{{"ok":true,"message":"accepted seq {}","fills":"{}","snapshot":{}}}"#,
                    accepted.event.seq,
                    json_escape(&protocol::format_fills(&accepted.fills)),
                    snapshot_json(&engine.snapshot())
                ),
            },
            Err(err) => WebResponse {
                status: "500 Internal Server Error",
                body: format!(
                    r#"{{"ok":false,"message":"{}"}}"#,
                    json_escape(&err.to_string())
                ),
            },
        },
        Err(err) => WebResponse {
            status: "400 Bad Request",
            body: format!(
                r#"{{"ok":false,"message":"{}"}}"#,
                json_escape(&err.to_string())
            ),
        },
    }
}

fn write_http(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn snapshot_json(snapshot: &slipstream::BookSnapshot) -> String {
    format!(
        r#"{{"bids":{},"asks":{}}}"#,
        side_json(&snapshot.bids),
        side_json(&snapshot.asks)
    )
}

fn side_json(levels: &[(slipstream::Price, Vec<slipstream::OrderView>)]) -> String {
    let levels = levels
        .iter()
        .map(|(price, orders)| {
            let qty: slipstream::Quantity = orders.iter().map(|order| order.qty).sum();
            format!(
                r#"{{"price":{price},"qty":{qty},"orders":{}}}"#,
                orders.len()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{levels}]")
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

const WEB_APP: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Slipstream Trading Console</title>
  <style>
    :root {
      color-scheme: dark;
      --bg: #101214;
      --panel: #181c20;
      --panel-2: #20262b;
      --line: #303942;
      --text: #eef3f7;
      --muted: #93a1ad;
      --bid: #36c486;
      --ask: #f46f6f;
      --accent: #5fa8ff;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      font-family: Inter, ui-sans-serif, system-ui, Segoe UI, Arial, sans-serif;
      background: var(--bg);
      color: var(--text);
    }
    .app { min-height: 100vh; display: grid; grid-template-rows: auto 1fr; }
    header {
      display: flex;
      justify-content: space-between;
      gap: 16px;
      padding: 16px 20px;
      border-bottom: 1px solid var(--line);
      background: #12161a;
    }
    h1 { margin: 0; font-size: 20px; letter-spacing: 0; }
    .status { color: var(--muted); font-size: 13px; align-self: center; }
    main {
      display: grid;
      grid-template-columns: minmax(280px, 360px) minmax(320px, 1fr) minmax(280px, 420px);
      gap: 16px;
      padding: 16px;
    }
    section {
      min-width: 0;
      border: 1px solid var(--line);
      background: var(--panel);
      border-radius: 8px;
      overflow: hidden;
    }
    .section-head {
      display: flex;
      justify-content: space-between;
      align-items: center;
      gap: 12px;
      padding: 12px 14px;
      border-bottom: 1px solid var(--line);
      background: var(--panel-2);
      font-weight: 700;
    }
    form { display: grid; gap: 12px; padding: 14px; }
    label {
      display: grid;
      gap: 6px;
      color: var(--muted);
      font-size: 12px;
      text-transform: uppercase;
    }
    input, select, button {
      width: 100%;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: #0f1317;
      color: var(--text);
      padding: 10px;
      font: inherit;
    }
    button { cursor: pointer; border: 0; font-weight: 800; }
    .buy { background: var(--bid); color: #07120d; }
    .sell { background: var(--ask); color: #170707; }
    .neutral { background: var(--accent); color: #06101d; }
    .danger { background: #3a2024; color: #ffd6d6; border: 1px solid #7a333d; }
    .row { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
    .book { display: grid; grid-template-columns: 1fr 1fr; min-height: 420px; }
    .side { min-width: 0; border-right: 1px solid var(--line); }
    .side:last-child { border-right: 0; }
    .side h2 {
      margin: 0;
      padding: 10px 12px;
      font-size: 13px;
      color: var(--muted);
      border-bottom: 1px solid var(--line);
    }
    .level {
      display: grid;
      grid-template-columns: 1fr 1fr 70px;
      align-items: center;
      gap: 8px;
      padding: 9px 12px;
      border-bottom: 1px solid #242b31;
      font-variant-numeric: tabular-nums;
    }
    .price.bid { color: var(--bid); }
    .price.ask { color: var(--ask); }
    .empty { padding: 24px 12px; color: var(--muted); font-size: 14px; }
    .tape {
      display: flex;
      flex-direction: column;
      gap: 8px;
      padding: 12px;
      max-height: 520px;
      overflow: auto;
    }
    .event {
      border: 1px solid var(--line);
      border-radius: 6px;
      padding: 9px;
      background: #11161a;
      color: var(--muted);
      overflow-wrap: anywhere;
      font-size: 13px;
    }
    .event strong { color: var(--text); }
    @media (max-width: 900px) {
      main { grid-template-columns: 1fr; }
      .book { min-height: 280px; }
    }
  </style>
</head>
<body>
  <div class="app">
    <header>
      <div>
        <h1>Slipstream Trading Console</h1>
        <div class="status" id="connection">Connecting to local engine...</div>
      </div>
      <div class="status">Local mock primary</div>
    </header>
    <main>
      <section>
        <div class="section-head">Order Ticket</div>
        <form id="order-form">
          <label>Side
            <select id="side">
              <option>BID</option>
              <option>ASK</option>
            </select>
          </label>
          <div class="row">
            <label>Order ID
              <input id="order-id" type="number" min="1" value="1">
            </label>
            <label>Quantity
              <input id="qty" type="number" min="1" value="10">
            </label>
          </div>
          <label>Limit Price
            <input id="price" type="number" value="101">
          </label>
          <div class="row">
            <button class="buy" type="button" id="buy">Buy</button>
            <button class="sell" type="button" id="sell">Sell</button>
          </div>
        </form>
        <form id="cancel-form">
          <label>Cancel Order ID
            <input id="cancel-id" type="number" min="1" value="1">
          </label>
          <button class="danger" type="submit">Cancel Order</button>
        </form>
      </section>

      <section>
        <div class="section-head">
          <span>Market Depth</span>
          <button class="neutral" id="refresh" style="max-width:120px">Refresh</button>
        </div>
        <div class="book">
          <div class="side">
            <h2>Bids</h2>
            <div id="bids"></div>
          </div>
          <div class="side">
            <h2>Asks</h2>
            <div id="asks"></div>
          </div>
        </div>
      </section>

      <section>
        <div class="section-head">Activity Tape</div>
        <div class="tape" id="tape"></div>
      </section>
    </main>
  </div>

  <script>
    const tape = document.querySelector('#tape');
    const connection = document.querySelector('#connection');

    document.querySelector('#buy').addEventListener('click', () => submitLimit('BID'));
    document.querySelector('#sell').addEventListener('click', () => submitLimit('ASK'));
    document.querySelector('#refresh').addEventListener('click', refresh);
    document.querySelector('#cancel-form').addEventListener('submit', event => {
      event.preventDefault();
      sendCommand(`CANCEL ${document.querySelector('#cancel-id').value}`);
    });

    function submitLimit(side) {
      document.querySelector('#side').value = side;
      const id = document.querySelector('#order-id').value;
      const price = document.querySelector('#price').value;
      const qty = document.querySelector('#qty').value;
      sendCommand(`LIMIT ${side} ${id} ${price} ${qty}`);
      document.querySelector('#order-id').value = Number(id) + 1;
    }

    async function sendCommand(command) {
      try {
        const response = await fetch('/api/order', { method: 'POST', body: command });
        const data = await response.json();
        addTape(command, data);
        if (data.snapshot) renderBook(data.snapshot);
        await refresh();
      } catch (error) {
        addTape(command, { ok: false, message: error.message });
      }
    }

    async function refresh() {
      try {
        const response = await fetch('/api/snapshot');
        const snapshot = await response.json();
        renderBook(snapshot);
        connection.textContent = 'Connected';
      } catch (error) {
        connection.textContent = `Disconnected: ${error.message}`;
      }
    }

    function renderBook(snapshot) {
      renderSide('#bids', snapshot.bids, 'bid');
      renderSide('#asks', snapshot.asks, 'ask');
    }

    function renderSide(selector, levels, side) {
      const root = document.querySelector(selector);
      if (!levels.length) {
        root.innerHTML = '<div class="empty">No resting liquidity</div>';
        return;
      }
      root.innerHTML = levels.map(level => `
        <div class="level">
          <span class="price ${side}">${level.price}</span>
          <span>${level.qty}</span>
          <span>${level.orders} orders</span>
        </div>
      `).join('');
    }

    function addTape(command, data) {
      const item = document.createElement('div');
      item.className = 'event';
      item.innerHTML = `<strong>${data.ok ? 'OK' : 'ERR'}</strong> ${escapeHtml(command)}<br>${escapeHtml(data.message || '')}<br>${escapeHtml(data.fills || '')}`;
      tape.prepend(item);
    }

    function escapeHtml(value) {
      return String(value).replace(/[&<>"']/g, char => ({
        '&': '&amp;',
        '<': '&lt;',
        '>': '&gt;',
        '"': '&quot;',
        "'": '&#39;'
      }[char]));
    }

    refresh();
  </script>
</body>
</html>
"#;

fn run_demo() -> Result<(), Box<dyn std::error::Error>> {
    let log_path = PathBuf::from("target/slipstream-demo.events");
    if log_path.exists() {
        std::fs::remove_file(&log_path)?;
    }

    let commands = [
        Command::Limit {
            id: 1,
            side: Side::Ask,
            price: 101,
            qty: 10,
        },
        Command::Limit {
            id: 2,
            side: Side::Ask,
            price: 102,
            qty: 5,
        },
        Command::Limit {
            id: 3,
            side: Side::Bid,
            price: 102,
            qty: 12,
        },
        Command::Limit {
            id: 4,
            side: Side::Bid,
            price: 100,
            qty: 7,
        },
    ];

    let mut engine = Engine::new();
    let mut fills = Vec::new();

    for command in commands {
        let accepted = engine.submit(command)?;
        EventLog::append(&log_path, &accepted.event)?;
        fills.extend(accepted.fills);
    }

    let (recovered, recovered_fills) = EventLog::recover(&log_path)?;

    println!("Slipstream demo");
    println!("event log: {}", log_path.display());
    println!("fills: {fills:#?}");
    println!("live book: {:#?}", engine.snapshot());
    println!("recovered book: {:#?}", recovered.snapshot());
    println!("recovered fills match: {}", fills == recovered_fills);

    Ok(())
}

fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:7000".to_owned());
    let log_path = std::env::args()
        .nth(3)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/slipstream-primary.events"));

    let (engine, _) = if log_path.exists() {
        EventLog::recover(&log_path)?
    } else {
        (Engine::new(), Vec::new())
    };
    let engine = Arc::new(Mutex::new(engine));
    let listener = TcpListener::bind(&addr)?;

    println!("Slipstream primary listening on {addr}");
    println!("event log: {}", log_path.display());
    println!("commands: LIMIT <BID|ASK> <id> <price> <qty>, CANCEL <id>, SNAPSHOT, QUIT");

    for stream in listener.incoming() {
        let stream = stream?;
        let engine = Arc::clone(&engine);
        let log_path = log_path.clone();
        thread::spawn(move || {
            if let Err(err) = handle_client(stream, engine, log_path) {
                eprintln!("client error: {err}");
            }
        });
    }

    Ok(())
}

fn handle_client(
    mut stream: TcpStream,
    engine: Arc<Mutex<Engine>>,
    log_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(
        stream,
        "OK slipstream ready; send LIMIT, CANCEL, SNAPSHOT, or QUIT"
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }

        let request = line.trim();
        if request.is_empty() {
            continue;
        }

        if request.eq_ignore_ascii_case("QUIT") {
            writeln!(stream, "OK bye")?;
            break;
        }

        if request.eq_ignore_ascii_case("SNAPSHOT") {
            let snapshot = engine.lock().expect("engine mutex poisoned").snapshot();
            let snapshot = protocol::format_snapshot(&snapshot).replace('\n', " ");
            writeln!(stream, "OK {snapshot}")?;
            continue;
        }

        let command = match protocol::parse_command(request) {
            Ok(command) => command,
            Err(err) => {
                writeln!(stream, "ERR {err}")?;
                continue;
            }
        };

        let response = {
            let mut engine = engine.lock().expect("engine mutex poisoned");
            match engine.submit(command) {
                Ok(accepted) => {
                    EventLog::append(&log_path, &accepted.event)?;
                    format!(
                        "OK seq={} {}",
                        accepted.event.seq,
                        protocol::format_fills(&accepted.fills)
                    )
                }
                Err(err) => format!("ERR {err}"),
            }
        };

        writeln!(stream, "{response}")?;
    }

    Ok(())
}

fn run_submit() -> Result<(), Box<dyn std::error::Error>> {
    let Some(addr) = std::env::args().nth(2) else {
        print_usage();
        return Ok(());
    };
    let request = std::env::args().skip(3).collect::<Vec<_>>().join(" ");
    if request.is_empty() {
        print_usage();
        return Ok(());
    }

    let mut stream = TcpStream::connect(addr)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    print!("{response}");

    writeln!(stream, "{request}")?;
    response.clear();
    reader.read_line(&mut response)?;
    print!("{response}");

    Ok(())
}

fn run_recover() -> Result<(), Box<dyn std::error::Error>> {
    let log_path = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/slipstream-primary.events"));
    let (engine, fills) = EventLog::recover(&log_path)?;

    println!("recovered log: {}", log_path.display());
    println!("fills: {}", protocol::format_fills(&fills));
    println!("{}", protocol::format_snapshot(&engine.snapshot()));

    Ok(())
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  cargo run -- demo");
    eprintln!("  cargo run -- serve [addr] [log_path]");
    eprintln!("  cargo run -- web [addr] [log_path]");
    eprintln!("  cargo run -- submit <addr> LIMIT <BID|ASK> <id> <price> <qty>");
    eprintln!("  cargo run -- submit <addr> CANCEL <id>");
    eprintln!("  cargo run -- submit <addr> SNAPSHOT");
    eprintln!("  cargo run -- recover [log_path]");
}
