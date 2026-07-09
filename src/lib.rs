use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::{Display, Formatter};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

pub type OrderId = u64;
pub type Price = i64;
pub type Quantity = i64;
pub type Sequence = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Limit {
        id: OrderId,
        side: Side,
        price: Price,
        qty: Quantity,
    },
    Cancel {
        id: OrderId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub seq: Sequence,
    pub command: Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fill {
    pub seq: Sequence,
    pub maker: OrderId,
    pub taker: OrderId,
    pub price: Price,
    pub qty: Quantity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Accepted {
    pub event: Event,
    pub fills: Vec<Fill>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineError {
    DuplicateOrder(OrderId),
    NonPositiveQuantity(Quantity),
}

impl Display for EngineError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateOrder(id) => write!(f, "duplicate order id {id}"),
            Self::NonPositiveQuantity(qty) => write!(f, "quantity must be positive, got {qty}"),
        }
    }
}

impl std::error::Error for EngineError {}

#[derive(Debug)]
pub enum LogError {
    Io(std::io::Error),
    MalformedLine {
        line: usize,
        text: String,
        reason: &'static str,
    },
    Engine {
        line: usize,
        source: EngineError,
    },
}

impl Display for LogError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => write!(f, "log IO error: {source}"),
            Self::MalformedLine { line, reason, .. } => {
                write!(f, "malformed log line {line}: {reason}")
            }
            Self::Engine { line, source } => {
                write!(f, "engine rejected event on log line {line}: {source}")
            }
        }
    }
}

impl std::error::Error for LogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Engine { source, .. } => Some(source),
            Self::MalformedLine { .. } => None,
        }
    }
}

impl From<std::io::Error> for LogError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderView {
    pub id: OrderId,
    pub qty: Quantity,
    pub seq: Sequence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookSnapshot {
    pub bids: Vec<(Price, Vec<OrderView>)>,
    pub asks: Vec<(Price, Vec<OrderView>)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RestingOrder {
    id: OrderId,
    qty: Quantity,
    seq: Sequence,
}

#[derive(Clone, Debug, Default)]
pub struct Engine {
    next_seq: Sequence,
    bids: BTreeMap<Price, VecDeque<RestingOrder>>,
    asks: BTreeMap<Price, VecDeque<RestingOrder>>,
    order_index: HashMap<OrderId, (Side, Price)>,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            ..Self::default()
        }
    }

    pub fn submit(&mut self, command: Command) -> Result<Accepted, EngineError> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let event = Event { seq, command };
        self.apply(event)
    }

    pub fn apply(&mut self, event: Event) -> Result<Accepted, EngineError> {
        self.next_seq = self.next_seq.max(event.seq + 1);

        let fills = match event.command.clone() {
            Command::Limit {
                id,
                side,
                price,
                qty,
            } => self.apply_limit(event.seq, id, side, price, qty)?,
            Command::Cancel { id } => {
                self.cancel(id);
                Vec::new()
            }
        };

        Ok(Accepted { event, fills })
    }

    pub fn replay(events: &[Event]) -> Result<(Self, Vec<Fill>), EngineError> {
        let mut engine = Self::new();
        let mut fills = Vec::new();

        for event in events {
            fills.extend(engine.apply(event.clone())?.fills);
        }

        Ok((engine, fills))
    }

    pub fn snapshot(&self) -> BookSnapshot {
        BookSnapshot {
            bids: self
                .bids
                .iter()
                .rev()
                .map(|(price, orders)| (*price, view_orders(orders)))
                .collect(),
            asks: self
                .asks
                .iter()
                .map(|(price, orders)| (*price, view_orders(orders)))
                .collect(),
        }
    }

    fn apply_limit(
        &mut self,
        seq: Sequence,
        id: OrderId,
        side: Side,
        price: Price,
        mut qty: Quantity,
    ) -> Result<Vec<Fill>, EngineError> {
        if qty <= 0 {
            return Err(EngineError::NonPositiveQuantity(qty));
        }

        if self.order_index.contains_key(&id) {
            return Err(EngineError::DuplicateOrder(id));
        }

        let mut fills = Vec::new();
        match side {
            Side::Bid => {
                while qty > 0 {
                    let Some(best_ask) = self.asks.keys().next().copied() else {
                        break;
                    };
                    if best_ask > price {
                        break;
                    }
                    qty = self.match_level(seq, id, Side::Ask, best_ask, qty, &mut fills);
                }
            }
            Side::Ask => {
                while qty > 0 {
                    let Some(best_bid) = self.bids.keys().next_back().copied() else {
                        break;
                    };
                    if best_bid < price {
                        break;
                    }
                    qty = self.match_level(seq, id, Side::Bid, best_bid, qty, &mut fills);
                }
            }
        }

        if qty > 0 {
            self.rest(seq, id, side, price, qty);
        }

        Ok(fills)
    }

    fn match_level(
        &mut self,
        seq: Sequence,
        taker: OrderId,
        maker_side: Side,
        price: Price,
        mut taker_qty: Quantity,
        fills: &mut Vec<Fill>,
    ) -> Quantity {
        let book = match maker_side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };

        let level = book
            .get_mut(&price)
            .expect("best price came from the same book");

        while taker_qty > 0 {
            let Some(maker) = level.front_mut() else {
                break;
            };
            let traded = maker.qty.min(taker_qty);
            maker.qty -= traded;
            taker_qty -= traded;

            fills.push(Fill {
                seq,
                maker: maker.id,
                taker,
                price,
                qty: traded,
            });

            if maker.qty == 0 {
                let filled = level.pop_front().expect("maker just matched");
                self.order_index.remove(&filled.id);
            }
        }

        if level.is_empty() {
            book.remove(&price);
        }

        taker_qty
    }

    fn rest(&mut self, seq: Sequence, id: OrderId, side: Side, price: Price, qty: Quantity) {
        let order = RestingOrder { id, qty, seq };
        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };

        book.entry(price).or_default().push_back(order);
        self.order_index.insert(id, (side, price));
    }

    fn cancel(&mut self, id: OrderId) {
        let Some((side, price)) = self.order_index.remove(&id) else {
            return;
        };

        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let Some(level) = book.get_mut(&price) else {
            return;
        };
        let Some(pos) = level.iter().position(|order| order.id == id) else {
            return;
        };

        level.remove(pos);
        if level.is_empty() {
            book.remove(&price);
        }
    }
}

fn view_orders(orders: &VecDeque<RestingOrder>) -> Vec<OrderView> {
    orders
        .iter()
        .map(|order| OrderView {
            id: order.id,
            qty: order.qty,
            seq: order.seq,
        })
        .collect()
}

pub struct EventLog;

impl EventLog {
    pub fn append(path: impl AsRef<Path>, event: &Event) -> Result<(), LogError> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", encode_event(event))?;
        file.flush()?;
        Ok(())
    }

    pub fn read(path: impl AsRef<Path>) -> Result<Vec<Event>, LogError> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();

        for (idx, line) in reader.lines().enumerate() {
            let line_no = idx + 1;
            let text = line?;
            if text.trim().is_empty() {
                continue;
            }
            events.push(decode_event(line_no, &text)?);
        }

        Ok(events)
    }

    pub fn recover(path: impl AsRef<Path>) -> Result<(Engine, Vec<Fill>), LogError> {
        let events = Self::read(path)?;
        let mut engine = Engine::new();
        let mut fills = Vec::new();

        for (idx, event) in events.into_iter().enumerate() {
            let line = idx + 1;
            match engine.apply(event) {
                Ok(accepted) => fills.extend(accepted.fills),
                Err(source) => return Err(LogError::Engine { line, source }),
            }
        }

        Ok((engine, fills))
    }
}

fn encode_event(event: &Event) -> String {
    match event.command {
        Command::Limit {
            id,
            side,
            price,
            qty,
        } => format!(
            "{}|L|{}|{}|{}|{}",
            event.seq,
            encode_side(side),
            id,
            price,
            qty
        ),
        Command::Cancel { id } => format!("{}|C|{}", event.seq, id),
    }
}

fn decode_event(line: usize, text: &str) -> Result<Event, LogError> {
    let fields: Vec<&str> = text.split('|').collect();
    let seq = parse_field(line, text, fields.first().copied(), "sequence")?;

    match fields.get(1).copied() {
        Some("L") if fields.len() == 6 => Ok(Event {
            seq,
            command: Command::Limit {
                side: decode_side(line, text, fields[2])?,
                id: parse_field(line, text, fields.get(3).copied(), "order id")?,
                price: parse_field(line, text, fields.get(4).copied(), "price")?,
                qty: parse_field(line, text, fields.get(5).copied(), "quantity")?,
            },
        }),
        Some("C") if fields.len() == 3 => Ok(Event {
            seq,
            command: Command::Cancel {
                id: parse_field(line, text, fields.get(2).copied(), "order id")?,
            },
        }),
        _ => Err(LogError::MalformedLine {
            line,
            text: text.to_owned(),
            reason: "expected limit line `seq|L|side|id|price|qty` or cancel line `seq|C|id`",
        }),
    }
}

fn parse_field<T: std::str::FromStr>(
    line: usize,
    text: &str,
    value: Option<&str>,
    reason: &'static str,
) -> Result<T, LogError> {
    value
        .and_then(|value| value.parse::<T>().ok())
        .ok_or_else(|| LogError::MalformedLine {
            line,
            text: text.to_owned(),
            reason,
        })
}

fn encode_side(side: Side) -> &'static str {
    match side {
        Side::Bid => "B",
        Side::Ask => "A",
    }
}

fn decode_side(line: usize, text: &str, value: &str) -> Result<Side, LogError> {
    match value {
        "B" => Ok(Side::Bid),
        "A" => Ok(Side::Ask),
        _ => Err(LogError::MalformedLine {
            line,
            text: text.to_owned(),
            reason: "side must be `B` or `A`",
        }),
    }
}

pub mod protocol {
    use std::fmt::{Display, Formatter};

    use crate::{BookSnapshot, Command, Fill, Price, Quantity, Side};

    #[derive(Debug, Eq, PartialEq)]
    pub enum ProtocolError {
        Empty,
        UnknownCommand(String),
        InvalidArity {
            command: String,
            expected: &'static str,
        },
        InvalidSide(String),
        InvalidNumber {
            field: &'static str,
            value: String,
        },
    }

    impl Display for ProtocolError {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Empty => write!(f, "empty command"),
                Self::UnknownCommand(command) => write!(f, "unknown command `{command}`"),
                Self::InvalidArity { command, expected } => {
                    write!(f, "`{command}` expects {expected}")
                }
                Self::InvalidSide(side) => write!(f, "side must be BID or ASK, got `{side}`"),
                Self::InvalidNumber { field, value } => {
                    write!(f, "{field} must be a number, got `{value}`")
                }
            }
        }
    }

    impl std::error::Error for ProtocolError {}

    pub fn parse_command(line: &str) -> Result<Command, ProtocolError> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some(kind) = parts.first().map(|part| part.to_ascii_uppercase()) else {
            return Err(ProtocolError::Empty);
        };

        match kind.as_str() {
            "LIMIT" => {
                if parts.len() != 5 {
                    return Err(ProtocolError::InvalidArity {
                        command: kind,
                        expected: "LIMIT <BID|ASK> <id> <price> <qty>",
                    });
                }

                Ok(Command::Limit {
                    side: parse_side(parts[1])?,
                    id: parse_u64("id", parts[2])?,
                    price: parse_i64("price", parts[3])?,
                    qty: parse_i64("qty", parts[4])?,
                })
            }
            "CANCEL" => {
                if parts.len() != 2 {
                    return Err(ProtocolError::InvalidArity {
                        command: kind,
                        expected: "CANCEL <id>",
                    });
                }

                Ok(Command::Cancel {
                    id: parse_u64("id", parts[1])?,
                })
            }
            "SNAPSHOT" => {
                if parts.len() == 1 {
                    Err(ProtocolError::UnknownCommand(kind))
                } else {
                    Err(ProtocolError::InvalidArity {
                        command: kind,
                        expected: "SNAPSHOT",
                    })
                }
            }
            _ => Err(ProtocolError::UnknownCommand(kind)),
        }
    }

    pub fn format_snapshot(snapshot: &BookSnapshot) -> String {
        let bids = format_side(&snapshot.bids);
        let asks = format_side(&snapshot.asks);
        format!("BIDS {bids}\nASKS {asks}")
    }

    pub fn format_fills(fills: &[Fill]) -> String {
        if fills.is_empty() {
            return "fills=[]".to_owned();
        }

        let rendered = fills
            .iter()
            .map(|fill| {
                format!(
                    "{{seq={},maker={},taker={},price={},qty={}}}",
                    fill.seq, fill.maker, fill.taker, fill.price, fill.qty
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        format!("fills=[{rendered}]")
    }

    fn format_side(levels: &[(Price, Vec<crate::OrderView>)]) -> String {
        if levels.is_empty() {
            return "[]".to_owned();
        }

        let rendered = levels
            .iter()
            .map(|(price, orders)| {
                let qty: Quantity = orders.iter().map(|order| order.qty).sum();
                format!("{price}@{qty}")
            })
            .collect::<Vec<_>>()
            .join(",");

        format!("[{rendered}]")
    }

    fn parse_side(value: &str) -> Result<Side, ProtocolError> {
        match value.to_ascii_uppercase().as_str() {
            "BID" | "B" => Ok(Side::Bid),
            "ASK" | "A" => Ok(Side::Ask),
            _ => Err(ProtocolError::InvalidSide(value.to_owned())),
        }
    }

    fn parse_u64(field: &'static str, value: &str) -> Result<u64, ProtocolError> {
        value.parse().map_err(|_| ProtocolError::InvalidNumber {
            field,
            value: value.to_owned(),
        })
    }

    fn parse_i64(field: &'static str, value: &str) -> Result<i64, ProtocolError> {
        value.parse().map_err(|_| ProtocolError::InvalidNumber {
            field,
            value: value.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn replay_rebuilds_identical_book_and_fills() {
        // Catches nondeterminism in matching order, sequence assignment, or book rebuild.
        let commands = vec![
            Command::Limit {
                id: 1,
                side: Side::Ask,
                price: 101,
                qty: 10,
            },
            Command::Limit {
                id: 2,
                side: Side::Ask,
                price: 101,
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

        let mut live = Engine::new();
        let mut events = Vec::new();
        let mut live_fills = Vec::new();

        for command in commands {
            let accepted = live.submit(command).expect("command should be accepted");
            events.push(accepted.event);
            live_fills.extend(accepted.fills);
        }

        let (replayed, replayed_fills) = Engine::replay(&events).expect("replay should succeed");
        assert_eq!(
            live.snapshot(),
            replayed.snapshot(),
            "replayed book differs"
        );
        assert_eq!(live_fills, replayed_fills, "replayed fills differ");
    }

    #[test]
    fn price_time_priority_fills_fifo_at_same_price() {
        // Catches engines that prioritize by size, id, or timestamp instead of arrival sequence.
        let mut engine = Engine::new();
        engine
            .submit(Command::Limit {
                id: 10,
                side: Side::Ask,
                price: 100,
                qty: 5,
            })
            .unwrap();
        engine
            .submit(Command::Limit {
                id: 11,
                side: Side::Ask,
                price: 100,
                qty: 5,
            })
            .unwrap();

        let accepted = engine
            .submit(Command::Limit {
                id: 12,
                side: Side::Bid,
                price: 100,
                qty: 6,
            })
            .unwrap();

        assert_eq!(
            accepted.fills,
            vec![
                Fill {
                    seq: 3,
                    maker: 10,
                    taker: 12,
                    price: 100,
                    qty: 5,
                },
                Fill {
                    seq: 3,
                    maker: 11,
                    taker: 12,
                    price: 100,
                    qty: 1,
                },
            ],
            "same-price orders must fill FIFO"
        );
    }

    #[test]
    fn partial_fill_sweeps_best_prices_and_rests_remainder() {
        // Catches wrong price ordering and incorrect remaining quantity after a sweep.
        let mut engine = Engine::new();
        engine
            .submit(Command::Limit {
                id: 1,
                side: Side::Ask,
                price: 100,
                qty: 3,
            })
            .unwrap();
        engine
            .submit(Command::Limit {
                id: 2,
                side: Side::Ask,
                price: 101,
                qty: 4,
            })
            .unwrap();

        let accepted = engine
            .submit(Command::Limit {
                id: 3,
                side: Side::Bid,
                price: 102,
                qty: 10,
            })
            .unwrap();

        assert_eq!(
            accepted.fills,
            vec![
                Fill {
                    seq: 3,
                    maker: 1,
                    taker: 3,
                    price: 100,
                    qty: 3,
                },
                Fill {
                    seq: 3,
                    maker: 2,
                    taker: 3,
                    price: 101,
                    qty: 4,
                },
            ],
            "aggressor should sweep asks from best to worse"
        );
        assert_eq!(
            engine.snapshot().bids,
            vec![(
                102,
                vec![OrderView {
                    id: 3,
                    qty: 3,
                    seq: 3,
                }],
            )],
            "unfilled remainder should rest at its limit price"
        );
    }

    #[test]
    fn cancel_removes_only_the_target_order() {
        // Catches cancel logic that disturbs FIFO neighbors at the same level.
        let mut engine = Engine::new();
        for id in 1..=3 {
            engine
                .submit(Command::Limit {
                    id,
                    side: Side::Bid,
                    price: 100,
                    qty: 10,
                })
                .unwrap();
        }

        engine.submit(Command::Cancel { id: 2 }).unwrap();

        assert_eq!(
            engine.snapshot().bids,
            vec![(
                100,
                vec![
                    OrderView {
                        id: 1,
                        qty: 10,
                        seq: 1,
                    },
                    OrderView {
                        id: 3,
                        qty: 10,
                        seq: 3,
                    },
                ],
            )],
            "cancel should remove only the requested order"
        );
    }

    #[test]
    fn marketable_limit_never_leaves_crossed_book() {
        // Catches implementations that rest a crossing limit instead of matching it first.
        let mut engine = Engine::new();
        engine
            .submit(Command::Limit {
                id: 1,
                side: Side::Ask,
                price: 100,
                qty: 5,
            })
            .unwrap();
        engine
            .submit(Command::Limit {
                id: 2,
                side: Side::Bid,
                price: 101,
                qty: 5,
            })
            .unwrap();

        let snapshot = engine.snapshot();
        assert!(snapshot.bids.is_empty(), "crossing bid should not rest");
        assert!(snapshot.asks.is_empty(), "matched ask should be gone");
    }

    #[test]
    fn event_log_recovers_exact_book_and_fills_after_restart() {
        // Catches persistence formats that cannot rebuild the precise pre-crash state.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.log");
        let mut live = Engine::new();
        let mut live_fills = Vec::new();

        for command in [
            Command::Limit {
                id: 1,
                side: Side::Bid,
                price: 99,
                qty: 8,
            },
            Command::Limit {
                id: 2,
                side: Side::Ask,
                price: 101,
                qty: 6,
            },
            Command::Limit {
                id: 3,
                side: Side::Bid,
                price: 101,
                qty: 4,
            },
            Command::Cancel { id: 1 },
        ] {
            let accepted = live.submit(command).expect("command accepted");
            EventLog::append(&path, &accepted.event).expect("event appended");
            live_fills.extend(accepted.fills);
        }

        let (recovered, recovered_fills) = EventLog::recover(&path).expect("log recovered");
        assert_eq!(
            live.snapshot(),
            recovered.snapshot(),
            "recovered book should match live book"
        );
        assert_eq!(
            live_fills, recovered_fills,
            "recovered fills should match live fills"
        );
    }

    #[test]
    fn event_log_rejects_malformed_lines() {
        // Catches silent log corruption that would otherwise make recovery untrustworthy.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad.log");
        std::fs::write(&path, "1|L|B|42|100\n").expect("write bad log");

        let err = EventLog::read(&path).expect_err("malformed line should fail");
        assert!(
            matches!(err, LogError::MalformedLine { line: 1, .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn protocol_parses_limit_and_cancel_commands() {
        assert_eq!(
            protocol::parse_command("LIMIT BID 10 101 7").unwrap(),
            Command::Limit {
                id: 10,
                side: Side::Bid,
                price: 101,
                qty: 7,
            }
        );
        assert_eq!(
            protocol::parse_command("cancel 10").unwrap(),
            Command::Cancel { id: 10 }
        );
    }

    #[test]
    fn protocol_formats_snapshots_as_price_depth_levels() {
        let mut engine = Engine::new();
        engine
            .submit(Command::Limit {
                id: 1,
                side: Side::Bid,
                price: 100,
                qty: 5,
            })
            .unwrap();
        engine
            .submit(Command::Limit {
                id: 2,
                side: Side::Ask,
                price: 102,
                qty: 3,
            })
            .unwrap();

        assert_eq!(
            protocol::format_snapshot(&engine.snapshot()),
            "BIDS [100@5]\nASKS [102@3]"
        );
    }

    proptest! {
        #[test]
        fn replay_is_identical_for_random_valid_sequences(commands in command_stream()) {
            let mut live = Engine::new();
            let mut events = Vec::new();
            let mut live_fills = Vec::new();

            for command in commands {
                if let Ok(accepted) = live.submit(command) {
                    events.push(accepted.event);
                    live_fills.extend(accepted.fills);
                }
            }

            let (replayed, replayed_fills) = Engine::replay(&events).expect("accepted events should replay");
            prop_assert_eq!(live.snapshot(), replayed.snapshot());
            prop_assert_eq!(live_fills, replayed_fills);
            prop_assert!(!is_crossed(&live.snapshot()));
        }
    }

    fn command_stream() -> impl Strategy<Value = Vec<Command>> {
        prop::collection::vec(
            (
                1_u64..40,
                any::<bool>(),
                95_i64..106,
                1_i64..20,
                any::<bool>(),
            )
                .prop_map(|(id, is_bid, price, qty, is_cancel)| {
                    if is_cancel {
                        Command::Cancel { id }
                    } else {
                        Command::Limit {
                            id,
                            side: if is_bid { Side::Bid } else { Side::Ask },
                            price,
                            qty,
                        }
                    }
                }),
            0..80,
        )
    }

    fn is_crossed(snapshot: &BookSnapshot) -> bool {
        match (snapshot.bids.first(), snapshot.asks.first()) {
            (Some((bid, _)), Some((ask, _))) => bid >= ask,
            _ => false,
        }
    }
}
