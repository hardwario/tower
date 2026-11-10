//! The MQTT contract: `docs/cli/mqtt-api.md` held against the bridge.
//!
//! That page is what integrators write against, so it is read here as data. Its topic table, its
//! field tables, its JSON examples, its RPC and error tables and its property tables are parsed
//! out of the Markdown and compared with what the engine subscribes to, publishes, accepts and
//! answers in a scripted session (no broker, no dongle: the engine is a pure state machine). A
//! topic, a key, a type, a retain flag, an op, a parameter, a default, an error text or a property
//! changed on one side only fails a test here, so the page and the code cannot drift apart
//! silently. A change a subscriber would notice also needs `payload::SCHEMA` bumped: the page
//! says which.
//!
//! What is not held here: the prose that describes behavior over time (when a topic is
//! republished, what the node does with a command), and the numbers that live in the firmware
//! (the registry and queue capacities).

use std::collections::BTreeSet;

use serde_json::Value;

use super::*;

/// The page under test.
const DOC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/cli/mqtt-api.md"));

/// The scripted session's prefix (the default one) and its two nodes.
const P: &str = "tower/";
const A: u32 = 0x0000_AB12;
const B: u32 = 0x0000_CD34;

// ---- reading the page ---------------------------------------------------------------------------

/// The body under the heading titled `title` (the text after its `#`s), up to the next heading of
/// the same or a higher level. Fenced code is skipped when looking for headings: a `# comment` in
/// a shell example is not one.
fn section(title: &str) -> &'static str {
    let mut fenced = false;
    let mut start: Option<(usize, usize)> = None; // (body offset, heading level)
    let mut offset = 0;
    for line in DOC.split_inclusive('\n') {
        let at = offset;
        offset += line.len();
        if line.starts_with("```") {
            fenced = !fenced;
        }
        let level = line.bytes().take_while(|&b| b == b'#').count();
        if fenced || level == 0 || !line[level..].starts_with(' ') {
            continue;
        }
        match start {
            Some((body, own)) if level <= own => return &DOC[body..at],
            Some(_) => {}
            None if line[level..].trim() == title => start = Some((offset, level)),
            None => {}
        }
    }
    let (body, _) = start.unwrap_or_else(|| panic!("the page has no section titled {title:?}"));
    &DOC[body..]
}

/// One Markdown table: the header cells and the body rows, cells trimmed.
#[derive(Debug)]
struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// Every table in `text`, in order.
fn tables(text: &str) -> Vec<Table> {
    let mut out: Vec<Table> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        let line = line.trim();
        let Some(inner) = line.strip_prefix('|').and_then(|l| l.strip_suffix('|')) else {
            open = false;
            continue;
        };
        let cells: Vec<String> = inner.split('|').map(|c| c.trim().to_string()).collect();
        if !open {
            open = true;
            out.push(Table {
                header: cells,
                rows: Vec::new(),
            });
        } else if !cells.iter().all(|c| c.chars().all(|ch| ch == '-' || ch == ':')) {
            let table = out.last_mut().expect("opened above");
            assert_eq!(cells.len(), table.header.len(), "a ragged table row: {line}");
            table.rows.push(cells);
        }
    }
    out
}

/// The table of `text` whose first header cell is `header`.
fn table(text: &str, header: &str) -> Table {
    let mut found: Vec<Table> = tables(text)
        .into_iter()
        .filter(|t| t.header[0] == header)
        .collect();
    assert_eq!(found.len(), 1, "one table headed {header:?}");
    found.remove(0)
}

/// The table of `text` that has a row for `field` in its first column.
fn table_with(text: &str, field: &str) -> Table {
    tables(text)
        .into_iter()
        .find(|t| t.rows.iter().any(|r| code_spans(&r[0]).first() == Some(&field)))
        .unwrap_or_else(|| panic!("no table with a row for `{field}`"))
}

/// The `code spans` of a piece of Markdown, in order.
fn code_spans(text: &str) -> Vec<&str> {
    text.split('`').skip(1).step_by(2).collect()
}

/// Every fenced `json` example in `text`, parsed. An example that is not JSON fails here.
fn json_examples(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut block: Option<String> = None;
    for line in text.lines() {
        match &mut block {
            None if line.trim() == "```json" => block = Some(String::new()),
            None => {}
            Some(body) if line.trim() == "```" => {
                let v = serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"));
                out.push(v);
                block = None;
            }
            Some(body) => {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    out
}

/// `text` with every run of whitespace collapsed to one space, so a sentence can be looked up
/// across its line breaks.
fn unwrapped(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the JSON value `v` is of the type a field table's `Type` cell names (`string`,
/// `number or \`null\``, `bool, number or string`, `any`).
fn type_allows(cell: &str, v: &Value) -> bool {
    if cell.contains("any") {
        return true;
    }
    match v {
        Value::Null => cell.contains("null"),
        Value::Bool(_) => cell.contains("bool"),
        Value::Number(_) => cell.contains("number"),
        Value::String(_) => cell.contains("string"),
        Value::Array(_) | Value::Object(_) => cell.contains("object"),
    }
}

/// The keys of a JSON object.
fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"))
        .keys()
        .cloned()
        .collect()
}

/// The keys of a shape written as `{a, b: null, c}`.
fn shape_keys(shape: &str) -> BTreeSet<String> {
    shape
        .trim_matches(|c| c == '{' || c == '}')
        .split(',')
        .map(|k| k.split(':').next().unwrap_or("").trim().to_string())
        .collect()
}

/// One row of the topic table (section 2).
#[derive(Debug)]
struct TopicRow {
    /// The topic as written: `nodes/{addr}/state/{prop}`.
    pattern: String,
    by_gateway: bool,
    retained: bool,
    diagnostic: bool,
}

fn topic_rows() -> Vec<TopicRow> {
    let t = table(section("2. Topics"), "Topic");
    assert_eq!(t.header, ["Topic", "Published by", "Retained", "Payload"]);
    t.rows
        .iter()
        .map(|r| {
            let by_gateway = match r[1].as_str() {
                "gateway" | "gateway (+ last will)" => true,
                "client" => false,
                other => panic!("the topic table names an unknown publisher: {other}"),
            };
            // A client's publish is "never" retained; the gateway's is, or is not.
            let retained = match (by_gateway, r[2].as_str()) {
                (false, "**never**") | (true, "no") => false,
                (true, cell) if cell == "yes" || cell.starts_with("yes (empty = ") => true,
                (_, other) => panic!("the topic table has an unknown retain cell: {other}"),
            };
            TopicRow {
                pattern: code_spans(&r[0])[0].to_string(),
                by_gateway,
                retained,
                diagnostic: r[3].contains("(diagnostic)"),
            }
        })
        .collect()
}

/// Whether `topic` (without the prefix) is an instance of `pattern`: the same levels, a
/// `{placeholder}` standing for any one.
fn is_instance(pattern: &str, topic: &str) -> bool {
    let (p, t): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), topic.split('/').collect());
    p.len() == t.len() && p.iter().zip(&t).all(|(p, t)| p.starts_with('{') || p == t)
}

/// `pattern` as a subscription filter: every placeholder a `+`.
fn filter_of(pattern: &str) -> String {
    let levels: Vec<&str> = pattern
        .split('/')
        .map(|l| if l.starts_with('{') { "+" } else { l })
        .collect();
    levels.join("/")
}

/// A concrete topic for `pattern`, under the prefix.
fn sample_of(pattern: &str) -> String {
    let rel = pattern
        .replace("{addr}", &topics::node_hex(A))
        .replace("{prop}", "setpoint")
        .replace("{id}", "u1");
    assert!(!rel.contains('{'), "an unknown placeholder in {pattern}");
    format!("{P}{rel}")
}

/// The field table (name, type) of a gateway topic's payload: the first table of its own section,
/// or, for the two dialogs documented in running text, the table holding their marker field.
fn fields_of(pattern: &str) -> Vec<(String, String)> {
    let t = match pattern {
        "gateway/rsp/{id}" => table_with(section("4.1 Protocol"), "ok"),
        "nodes/{addr}/shell/rsp" => table_with(section("5. Remote shell"), "chunk"),
        _ => table(section(&format!("`{pattern}`")), "Field"),
    };
    assert_eq!(&t.header[..2], ["Field", "Type"], "{pattern}");
    t.rows
        .iter()
        .map(|r| (code_spans(&r[0])[0].to_string(), r[1].clone()))
        .collect()
}

// ---- driving the bridge -------------------------------------------------------------------------

/// The engine with everything it asked of the broker recorded.
struct Bridge {
    e: Engine,
    subs: Vec<String>,
    /// Every publish so far: topic (with the prefix), payload, retain.
    pubs: Vec<(String, Vec<u8>, bool)>,
}

impl Bridge {
    fn new() -> Self {
        Self {
            e: engine(),
            subs: Vec::new(),
            pubs: Vec::new(),
        }
    }

    /// A started bridge with node `A` ("kitchen", sleeping) in its registry.
    fn with_kitchen() -> Self {
        let mut b = Self::new();
        let out = b.e.start();
        let out = b.note(out);
        let out = b.answer(
            &out,
            MGMT_OK,
            &node_entry_record(A, "kitchen", NODE_FLAG_SLEEPING),
        );
        b.settle(out);
        b
    }

    fn note(&mut self, out: Vec<Output>) -> Vec<Output> {
        for o in &out {
            match o {
                Output::Publish {
                    topic,
                    payload,
                    retain,
                } => self.pubs.push((topic.clone(), payload.clone(), *retain)),
                Output::Subscribe(filter) => self.subs.push(filter.clone()),
                _ => {}
            }
        }
        out
    }

    fn handle(&mut self, input: Input) -> Vec<Output> {
        let out = self.e.handle(input);
        self.note(out)
    }

    fn mqtt(&mut self, topic: &str, payload: &[u8]) -> Vec<Output> {
        self.handle(mqtt_in(topic, payload))
    }

    /// One request on `gateway/cmd`.
    fn rpc(&mut self, id: &str, op: &str, params: &Value) -> Vec<Output> {
        let req = serde_json::json!({ "id": id, "op": op, "params": params });
        self.mqtt(&topics::gateway_cmd(P), req.to_string().as_bytes())
    }

    fn ticks(&mut self, n: u64) {
        for _ in 0..n {
            let out = self.handle(Input::Tick);
            self.settle(out);
        }
    }

    /// Answer the request `out` sent to the dongle.
    fn answer(&mut self, out: &[Output], result: u8, data: &[u8]) -> Vec<Output> {
        let (req_id, _) = out
            .iter()
            .find_map(sent_op)
            .expect("a request went to the dongle");
        let out = reply(&mut self.e, req_id, result, data);
        self.note(out)
    }

    /// Answer every request in flight (and those it releases) with an empty OK, a listing with the
    /// registry as mirrored, so the dongle's one request slot is free again.
    fn settle(&mut self, mut out: Vec<Output>) {
        for _ in 0..16 {
            let sent: Vec<(u16, String)> = out.iter().filter_map(sent_op).collect();
            if sent.is_empty() {
                return;
            }
            out = Vec::new();
            for (id, op) in sent {
                let data: Vec<u8> = if op.contains("NodeList") {
                    self.e
                        .nodes
                        .iter()
                        .flat_map(|n| stored_record(n.entry.addr, &n.entry.name, n.entry.flags, n.entry.kind))
                        .collect()
                } else {
                    Vec::new()
                };
                let answered = reply(&mut self.e, id, MGMT_OK, &data);
                out.extend(self.note(answered));
            }
        }
        panic!("the requests never settled");
    }

    /// A remote shell line for node `A`, accepted and queued on the dongle as `item`.
    fn queue_line(&mut self, id: &str, item: u16, ttl: u16) {
        let req = serde_json::json!({ "id": id, "line": "led on", "ttl": ttl });
        let out = self.mqtt(&topics::node_shell_req(P, A), req.to_string().as_bytes());
        let out = self.answer(&out, MGMT_OK, &postcard::to_stdvec(&QueueId { item }).unwrap());
        self.settle(out);
    }

    /// The publishes on `topic` since `mark` (an earlier `pubs.len()`), parsed.
    fn json_since(&self, mark: usize, topic: &str) -> Vec<Value> {
        self.pubs[mark..]
            .iter()
            .filter(|(t, payload, _)| t == topic && !payload.is_empty())
            .map(|(_, payload, _)| serde_json::from_slice(payload).expect("JSON"))
            .collect()
    }

    /// The one RPC response for `id`.
    fn response(&self, id: &str) -> Value {
        let mut found = self.json_since(0, &topics::gateway_rsp(P, id));
        assert_eq!(found.len(), 1, "one response for {id}");
        found.remove(0)
    }
}

/// Whether the bridge took a `set` of `prop` (checked it and queued it for the node) or refused
/// it; it says which in its log, and only a set it took goes to the dongle.
fn set_queued(out: &[Output], prop: &str) -> bool {
    let lines = logs(out);
    let taken = lines.iter().any(|l| l.starts_with(&format!("set {prop} = ")));
    let refused = lines
        .iter()
        .any(|l| l.starts_with(&format!("refusing set {prop} on ")));
    assert_ne!(taken, refused, "a set is taken or refused: {lines:?}");
    let pushed = out
        .iter()
        .find_map(sent_op)
        .is_some_and(|(_, op)| op.contains("QueuePush"));
    assert_eq!(taken, pushed, "{lines:?}");
    taken
}

fn tx_stat(item: u16, outcome: u8, ack_rssi: Option<i8>) -> Input {
    Input::SerialFrame(SerialMsg::Stat(RadioStat::Tx {
        dest: A,
        item,
        outcome,
        ack_rssi,
    }))
}

fn hello(session_id: u32) -> Input {
    Input::SerialFrame(SerialMsg::Hello {
        firmware_name: "radio_gateway".into(),
        firmware_version: "v0.1.0".into(),
        session_id,
    })
}

/// One session that makes the bridge publish on every topic it has: start, the broker link, a
/// dongle boot, radio statistics, a thermostat's heartbeat, report and event, a remote shell
/// dialog, a set, an OTA pairing, a refused request and a removal.
fn session() -> Bridge {
    let mut b = Bridge::with_kitchen();
    let out = b.handle(Input::MqttUp);
    b.settle(out);
    let out = b.handle(hello(7));
    b.settle(out);
    b.handle(Input::SerialFrame(SerialMsg::Stat(RadioStat::Channel {
        channel: 3,
        rssi: -98,
    })));
    b.ticks(STATS_EVERY_TICKS);

    // Node A is a thermostat: its heartbeat names the kind, then a report and an event.
    let out = b.handle(info_uplink(A, 1, "radio_thermostat"));
    b.settle(out);
    let mut items = heapless::Vec::new();
    items
        .extend_from_slice(&[
            (props::common::TEMPERATURE, 2150),
            (props::thermostat::SETPOINT, 215),
            (props::thermostat::MODE, 0),
            (props::thermostat::DEMAND, 1),
        ])
        .unwrap();
    b.handle(msg_uplink(A, 2, &NodeMsg::Report(items)));
    let click = NodeMsg::Event {
        id: props::common::BUTTON,
        value: 2,
        count: 7,
    };
    b.handle(msg_uplink(A, 3, &click));

    // A remote shell dialog: queued, delivered, answered. A refused one ends with `error`.
    b.queue_line("sh-1", 9, 0);
    b.handle(tx_stat(9, mgmt::TX_DELIVERED, Some(-42)));
    b.handle(tx_stat(0, mgmt::TX_NOT_DELIVERED, None));
    let chunk = NodeMsg::Shell(radio::NodeShellChunk {
        cmd_id: 1,
        result: 0,
        chunk: 0,
        last: true,
        text: "ok",
    });
    let out = b.handle(msg_uplink(A, 4, &chunk));
    b.settle(out);
    b.mqtt(
        &topics::node_shell_req(P, 0xFFFF),
        br#"{"id":"sh-2","line":"led on"}"#,
    );

    // A set, queued for the node's next uplink.
    let out = b.mqtt(&format!("{}/setpoint", sample_of("nodes/{addr}/set")), b"21.5");
    let out = b.answer(
        &out,
        MGMT_OK,
        &postcard::to_stdvec(&QueueId { item: 10 }).unwrap(),
    );
    b.settle(out);

    // An OTA pairing window: open, counting down, closed by node B's join.
    let out = b.rpc("r-ota", "node_add_ota", &serde_json::json!({ "window": 30 }));
    let (ota, _) = out.iter().find_map(sent_op).expect("PairingOpen goes out");
    b.handle(Input::Tick);
    let joined = postcard::to_stdvec(&Paired { addr: B, sas: 4321 }).unwrap();
    let out = reply(&mut b.e, ota, MGMT_OK, &joined);
    let out = b.note(out);
    b.settle(out);

    // A refused request, and node A removed: its retained topics are cleared.
    b.rpc("r-bad", "frobnicate", &Value::Null);
    let out = b.rpc("r-rm", "node_remove", &serde_json::json!({ "addr": "kitchen" }));
    let out = b.answer(&out, MGMT_OK, &[]);
    b.settle(out);
    b
}

// ---- topics -------------------------------------------------------------------------------------

/// Everything the bridge publishes is a row of the topic table, retained as the row says, JSON
/// (or the empty payload that clears a retained topic), in the diagnostic class the row names;
/// every row is published; node topics carry the address as `0x` and 8 lowercase hex digits.
#[test]
fn every_publish_is_a_documented_topic() {
    let rows = topic_rows();
    let b = session();
    let mut published = BTreeSet::new();
    for (topic, payload, retain) in &b.pubs {
        let rel = topic.strip_prefix(P).expect("every topic is under the prefix");
        let hits: Vec<&TopicRow> = rows
            .iter()
            .filter(|r| r.by_gateway && is_instance(&r.pattern, rel))
            .collect();
        assert_eq!(hits.len(), 1, "{rel} is not exactly one row of the topic table");
        let row = hits[0];
        assert_eq!(*retain, row.retained, "{rel}: retain");
        if payload.is_empty() {
            assert!(row.retained, "{rel}: only a retained topic is cleared");
        } else {
            serde_json::from_slice::<Value>(payload).unwrap_or_else(|e| panic!("{rel}: not JSON: {e}"));
        }
        assert_eq!(
            topics::is_diagnostic(P, topic),
            row.diagnostic,
            "{rel}: the diagnostic class"
        );
        assert_eq!(
            topics::classify(P, topic),
            topics::Inbound::Other,
            "{rel}: the bridge must not take its own publish for a request"
        );
        if let Some(addr) = rel.strip_prefix("nodes/").and_then(|t| t.split('/').next()) {
            let parsed = topics::parse_node_hex(addr).unwrap_or_else(|| panic!("{rel}: not an address"));
            assert_eq!(addr, topics::node_hex(parsed), "{rel}: lowercase, 8 digits");
            assert_eq!(topics::node_of(P, topic), Some(parsed));
        }
        published.insert(row.pattern.clone());
    }
    let documented: BTreeSet<String> = rows
        .iter()
        .filter(|r| r.by_gateway)
        .map(|r| r.pattern.clone())
        .collect();
    assert_eq!(
        published, documented,
        "the session publishes on every documented topic"
    );
}

/// The bridge subscribes to exactly the topics the table gives to clients, on start and on every
/// broker (re)connect, takes each for a request, and neither runs nor keeps a retained one.
#[test]
fn the_subscriptions_are_the_client_topics() {
    let rows = topic_rows();
    let want: BTreeSet<String> = rows
        .iter()
        .filter(|r| !r.by_gateway)
        .map(|r| format!("{P}{}", filter_of(&r.pattern)))
        .collect();
    assert!(!want.is_empty());
    let mut b = Bridge::with_kitchen();
    assert_eq!(b.subs.iter().cloned().collect::<BTreeSet<_>>(), want);
    b.subs.clear();
    let out = b.handle(Input::MqttUp);
    b.settle(out);
    assert_eq!(
        b.subs.iter().cloned().collect::<BTreeSet<_>>(),
        want,
        "again on a reconnect"
    );

    for row in rows.iter().filter(|r| !r.by_gateway) {
        let topic = sample_of(&row.pattern);
        assert_ne!(
            topics::classify(P, &topic),
            topics::Inbound::Other,
            "{topic} is a request"
        );
        assert!(!topics::is_diagnostic(P, &topic));
        // A retained copy (the broker's replay of a stored message) is ignored and cleared.
        let out = b.handle(Input::MqttIn {
            topic: topic.clone(),
            payload: br#"{"id":"replayed","op":"node_list","line":"led on","value":21.5}"#.to_vec(),
            retain: true,
        });
        assert!(
            out.iter().find_map(sent_op).is_none(),
            "{topic}: a retained request must not run"
        );
        let publishes: Vec<&Output> = out
            .iter()
            .filter(|o| matches!(o, Output::Publish { .. }))
            .collect();
        assert_eq!(
            publishes,
            [&Output::Publish {
                topic: topic.clone(),
                payload: Vec::new(),
                retain: true
            }],
            "{topic}: the retained copy is cleared, nothing else is published"
        );
    }
}

/// An address in a request topic may be upper case; the answer is on the lowercase topic.
#[test]
fn a_request_topic_takes_either_case_and_is_answered_in_lower_case() {
    let mut b = Bridge::with_kitchen();
    let line = br#"{"id":"case","line":"led on"}"#;
    let out = b.mqtt("tower/nodes/0X0000AB12/shell/req", line);
    assert!(
        out.iter()
            .find_map(sent_op)
            .is_some_and(|(_, op)| op.contains("QueuePush")),
        "the upper-case address names node A"
    );
    let mark = b.pubs.len();
    b.mqtt(
        "tower/nodes/0X0000FFFF/shell/req",
        br#"{"id":"case2","line":"led on"}"#,
    );
    let answers = b.json_since(mark, "tower/nodes/0x0000ffff/shell/rsp");
    assert_eq!(answers.len(), 1, "answered on the lowercase topic");
    // Anything but `0x` and exactly 8 hex digits is no address: no answer at all.
    for bad in ["0xab12", "0000ab12", "kitchen", "0x0000ab123"] {
        let mark = b.pubs.len();
        let out = b.mqtt(
            &format!("tower/nodes/{bad}/shell/req"),
            br#"{"id":"case3","line":"led on"}"#,
        );
        assert!(out.iter().find_map(sent_op).is_none(), "{bad}");
        assert_eq!(b.pubs.len(), mark, "{bad}");
    }
}

// ---- payloads -----------------------------------------------------------------------------------

/// Every payload the bridge publishes carries only the keys of its field table, each of the type
/// the table gives, and every key of every table is published by the session (so a key dropped
/// from the code fails here like one dropped from the page).
#[test]
fn every_payload_has_the_documented_fields() {
    let b = session();
    for row in topic_rows().iter().filter(|r| r.by_gateway) {
        let fields = fields_of(&row.pattern);
        assert!(!fields.is_empty(), "{}", row.pattern);
        let mut seen = BTreeSet::new();
        for (topic, payload, _) in &b.pubs {
            if payload.is_empty() || !is_instance(&row.pattern, topic.strip_prefix(P).unwrap_or(topic)) {
                continue;
            }
            let value: Value = serde_json::from_slice(payload).expect("JSON");
            // `shell/pending` is an array of entries; everything else is one object.
            let objects: Vec<&Value> = match &value {
                Value::Array(entries) => entries.iter().collect(),
                other => vec![other],
            };
            assert_eq!(
                value.is_array(),
                row.pattern.ends_with("shell/pending"),
                "{topic}: {value}"
            );
            for object in objects {
                for (key, v) in object.as_object().unwrap_or_else(|| panic!("{topic}: {object}")) {
                    let (_, ty) = fields
                        .iter()
                        .find(|(name, _)| name == key)
                        .unwrap_or_else(|| panic!("{topic}: the key `{key}` is not in the field table"));
                    assert!(type_allows(ty, v), "{topic}: `{key}` is {v}, documented as {ty}");
                    seen.insert(key.clone());
                }
            }
        }
        let documented: BTreeSet<String> = fields.iter().map(|(name, _)| name.clone()).collect();
        assert_eq!(seen, documented, "{}: the keys published", row.pattern);
    }
}

/// A typed payload read from an example and written back is the example: the example has exactly
/// the keys the code writes for that state, with values of its types.
fn round_trips<T: serde::Serialize + serde::de::DeserializeOwned>(example: &Value) {
    let typed: T = serde_json::from_value(example.clone())
        .unwrap_or_else(|e| panic!("the example does not parse as the payload type ({e}): {example}"));
    assert_eq!(&serde_json::to_value(&typed).unwrap(), example);
}

/// Every JSON example of a published payload is what the code would publish.
#[test]
fn the_payload_examples_are_real() {
    let examples = |pattern: &str| {
        let found = json_examples(section(&format!("`{pattern}`")));
        assert!(!found.is_empty(), "{pattern} has an example");
        found
    };
    // The status: the full one, and the last will, which is `Status::offline()` to the letter.
    let status = examples("gateway/status");
    assert_eq!(status.len(), 2);
    round_trips::<payload::Status>(&status[0]);
    assert_eq!(status[0]["schema"], payload::SCHEMA);
    assert_eq!(status[1], payload::Status::offline());
    let read: payload::Status = serde_json::from_value(status[1].clone()).expect("the will parses");
    assert_eq!((read.state.as_str(), read.schema), ("offline", payload::SCHEMA));

    examples("gateway/stats")
        .iter()
        .for_each(round_trips::<payload::Stats>);
    let pairing = examples("gateway/pairing");
    assert_eq!(pairing.len(), 3, "open, idle, idle with a join");
    pairing.iter().for_each(round_trips::<payload::Pairing>);
    examples("nodes/{addr}")
        .iter()
        .for_each(round_trips::<payload::Node>);
    examples("nodes/{addr}/state/{prop}")
        .iter()
        .for_each(round_trips::<payload::State>);
    examples("nodes/{addr}/event/{prop}")
        .iter()
        .for_each(round_trips::<payload::PropEvent>);
    examples("nodes/{addr}/uplink")
        .iter()
        .for_each(round_trips::<payload::UplinkDebug>);
    examples("nodes/{addr}/shell/pending")
        .iter()
        .for_each(round_trips::<Vec<PendingEntry>>);
    examples("radio/rssi")
        .iter()
        .for_each(round_trips::<payload::RadioRssi>);
    examples("radio/rx")
        .iter()
        .for_each(round_trips::<payload::RadioRx>);
    examples("radio/tx")
        .iter()
        .for_each(round_trips::<payload::RadioTx>);

    // The two dialogs: a request parses (its optional keys left out), a response round-trips.
    let mut requests = 0;
    for example in json_examples(section("4.1 Protocol")) {
        if example.get("op").is_some() {
            let req: RpcRequest = serde_json::from_value(example).expect("an RPC request");
            assert!(payload::valid_request_id(&req.id));
            requests += 1;
        } else {
            round_trips::<RpcResponse>(&example);
        }
    }
    assert_eq!(requests, 1);
    let mut lines = 0;
    for example in json_examples(section("5. Remote shell")) {
        if example.get("line").is_some() {
            let req: payload::ShellReq = serde_json::from_value(example).expect("a shell request");
            assert!(payload::valid_request_id(&req.id));
            assert!(req.line.len() <= radio::RADIO_SHELL_CHUNK);
            lines += 1;
        } else {
            round_trips::<payload::ShellRsp>(&example);
        }
    }
    assert_eq!(lines, 1);

    // And every other example on the page is at least JSON.
    assert!(json_examples(DOC).len() > 40);
}

/// The request tables: the keys a client sends are the keys the code reads, optional where the
/// page says so.
#[test]
fn the_request_tables_are_what_the_bridge_reads() {
    let field_names =
        |t: &Table| -> BTreeSet<String> { t.rows.iter().map(|r| code_spans(&r[0])[0].to_string()).collect() };
    // An RPC request: `id` and `op` required, `params` optional.
    let rpc = table_with(section("4.1 Protocol"), "op");
    assert_eq!(
        field_names(&rpc),
        keys(
            &serde_json::to_value(RpcRequest {
                id: "u1".into(),
                op: "describe".into(),
                params: Value::Null,
            })
            .unwrap()
        )
    );
    assert!(serde_json::from_str::<RpcRequest>(r#"{"id":"u1","op":"describe"}"#).is_ok());
    assert!(serde_json::from_str::<RpcRequest>(r#"{"id":"u1"}"#).is_err());
    assert!(serde_json::from_str::<RpcRequest>(r#"{"op":"describe"}"#).is_err());

    // A shell request: `id` and `line` required, `ttl` optional (0 = the default).
    let shell = table_with(section("5. Remote shell"), "line");
    assert_eq!(
        field_names(&shell),
        keys(
            &serde_json::to_value(payload::ShellReq {
                id: "u1".into(),
                line: "led on".into(),
                ttl: 0,
            })
            .unwrap()
        )
    );
    let req: payload::ShellReq = serde_json::from_str(r#"{"id":"u1","line":"led on"}"#).unwrap();
    assert_eq!(req.ttl, 0);
    assert!(serde_json::from_str::<payload::ShellReq>(r#"{"id":"u1"}"#).is_err());
    assert!(serde_json::from_str::<payload::ShellReq>(r#"{"line":"led on"}"#).is_err());
    assert!(
        serde_json::from_str::<payload::ShellReq>(r#"{"id":"u1","line":"x","ttl":65536}"#).is_err(),
        "a ttl over 65535 does not parse"
    );
}

// ---- RPC ----------------------------------------------------------------------------------------

/// The `op` names the engine's `on_rpc` matches on, read from its source: the one way to see an
/// op that was added to the code and not to the page.
fn ops_in_the_code() -> BTreeSet<String> {
    // (A Windows checkout may hold the file with CRLF line ends.)
    let source = include_str!("../../engine.rs").replace("\r\n", "\n");
    let body = source
        .split_once("fn on_rpc(")
        .expect("engine.rs has on_rpc")
        .1
        .split_once("\n    }\n")
        .expect("on_rpc ends")
        .0;
    let arms: BTreeSet<String> = body
        .lines()
        .filter_map(|l| l.strip_prefix("            \"")?.split_once("\" =>"))
        .map(|(op, _)| op.to_string())
        .collect();
    assert!(arms.len() >= 12, "the scan found only {arms:?}");
    arms
}

/// One row of the operations table (4.2).
struct OpRow {
    op: String,
    /// The parameter names (their `?` dropped).
    params: BTreeSet<String>,
    /// The `data` cell: `none`, `{a, b}`, `array of ...`.
    data: String,
}

fn op_rows() -> Vec<OpRow> {
    let t = table(section("4.2 Operations"), "`op`");
    assert_eq!(t.header, ["`op`", "`params`", "`data` on success"]);
    t.rows
        .iter()
        .map(|r| OpRow {
            op: code_spans(&r[0])[0].to_string(),
            params: code_spans(&r[1]).iter().map(|p| p.to_string()).collect(),
            data: r[2].clone(),
        })
        .collect()
}

/// A request using every documented parameter of `op`, what the dongle must then be asked
/// (pieces of the management op), and the dongle's answer.
fn op_case(op: &str) -> (Value, &'static [&'static str], Vec<u8>) {
    let addr = topics::node_hex(A);
    match op {
        "describe" => (Value::Null, &["Describe"], Vec::new()),
        "node_list" => (
            Value::Null,
            &["NodeList"],
            node_entry_record(A, "kitchen", NODE_FLAG_SLEEPING),
        ),
        "node_add" => (
            serde_json::json!({
                "addr": topics::node_hex(B),
                "key": "0123456789abcdef0001020304050607",
                "name": "pantry",
                "sleeping": false,
                "tx_floor": 7,
                "rx_floor": 9,
            }),
            &[
                "NodeAdd",
                "addr: 52532",
                "key: [1, 35, 69, 103, 137, 171, 205, 239, 0, 1, 2, 3, 4, 5, 6, 7]",
                "name: \"pantry\"",
                "flags: 0",
                "tx_floor: 7",
                "rx_floor: 9",
            ],
            Vec::new(),
        ),
        "node_add_ota" => (
            serde_json::json!({ "window": 30 }),
            &["PairingOpen", "window: 30"],
            postcard::to_stdvec(&Paired { addr: B, sas: 4321 }).unwrap(),
        ),
        "pairing_cancel" => (Value::Null, &["PairingCancel"], Vec::new()),
        "node_rename" => (
            serde_json::json!({ "addr": addr, "name": "hall" }),
            &["NodeUpdate", "addr: 43794", "name: Some(\"hall\")"],
            Vec::new(),
        ),
        "node_remove" => (
            serde_json::json!({ "addr": addr }),
            &["NodeRemove", "addr: 43794"],
            Vec::new(),
        ),
        "reveal_key" => (
            serde_json::json!({ "addr": addr }),
            &["NodeRevealKey", "addr: 43794"],
            postcard::to_stdvec(&NodeKey {
                addr: A,
                key: [0xA5; 16],
                tx_floor: 3073,
                rx_floor: 42,
            })
            .unwrap(),
        ),
        "queue_list" => (
            serde_json::json!({ "addr": addr }),
            &["QueueList", "node_addr: 43794"],
            postcard::to_stdvec(&QueueEntry {
                node_addr: A,
                item: 9,
                age: 12,
                ttl: 3600,
                data: &[],
            })
            .unwrap(),
        ),
        "queue_drop" => (
            serde_json::json!({ "addr": addr, "ref": 9 }),
            &["QueueDrop", "node_addr: 43794", "item: Some(9)"],
            Vec::new(),
        ),
        "stats_config" => (
            serde_json::json!({ "period_ms": 5000 }),
            &["StatsConfig", "channel_period_ms: 5000"],
            Vec::new(),
        ),
        // No management request: the bridge pulses the reset line itself.
        "reboot" => (Value::Null, &[], Vec::new()),
        other => panic!("the operations table has an op this test has no case for: {other}"),
    }
}

/// `data` of a successful response has the shape the operations table gives.
fn assert_data_shape(row: &OpRow, response: &Value) {
    let data = response.get("data");
    if row.data == "none" {
        assert!(data.is_none(), "{}: no data, got {response}", row.op);
        return;
    }
    let data = data.unwrap_or_else(|| panic!("{}: data missing in {response}", row.op));
    let items: Vec<&Value> = if row.data.starts_with("array of") {
        let items: Vec<&Value> = data.as_array().expect("an array").iter().collect();
        assert!(!items.is_empty(), "{}: the case answers with an element", row.op);
        items
    } else {
        vec![data]
    };
    let shapes: Vec<BTreeSet<String>> = if row.data.contains("[Node]") {
        vec![
            fields_of("nodes/{addr}")
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
        ]
    } else {
        code_spans(&row.data).into_iter().map(shape_keys).collect()
    };
    assert!(
        !shapes.is_empty(),
        "{}: unreadable data cell {}",
        row.op,
        row.data
    );
    for item in items {
        assert!(
            shapes.contains(&keys(item)),
            "{}: data {item} is not {}",
            row.op,
            row.data
        );
    }
}

/// The operations table against the engine: the same ops, each reading the parameters the table
/// lists, asking the dongle what it should, and answering with the `data` the table gives.
#[test]
fn the_operations_are_the_documented_ones() {
    let rows = op_rows();
    let documented: BTreeSet<String> = rows.iter().map(|r| r.op.clone()).collect();
    assert_eq!(documented.len(), rows.len(), "an op listed twice");
    assert_eq!(ops_in_the_code(), documented);

    for row in &rows {
        let (params, asked, answer) = op_case(&row.op);
        let sent: BTreeSet<String> = match &params {
            Value::Null => BTreeSet::new(),
            p => keys(p),
        };
        assert_eq!(
            sent, row.params,
            "{}: the case uses every documented parameter",
            row.op
        );

        let mut b = Bridge::with_kitchen();
        let id = format!("c-{}", row.op);
        let out = b.rpc(&id, &row.op, &params);
        if asked.is_empty() {
            assert!(out.contains(&Output::SerialReset), "{}", row.op);
            assert!(out.iter().find_map(sent_op).is_none(), "{}", row.op);
        } else {
            let (_, op) = out
                .iter()
                .find_map(sent_op)
                .unwrap_or_else(|| panic!("{}: nothing asked of the dongle", row.op));
            for piece in asked {
                assert!(op.contains(piece), "{}: {piece} not in {op}", row.op);
            }
            let out = b.answer(&out, MGMT_OK, &answer);
            b.settle(out);
        }
        let response = b.response(&id);
        assert_eq!(response["id"], id.as_str());
        assert_eq!(response["ok"], true, "{}: {response}", row.op);
        assert!(response.get("error").is_none(), "{}: {response}", row.op);
        assert_data_shape(row, &response);

        // The op's own section repeats its parameters: the same names.
        let own = section(&format!("`{}`", row.op));
        if let Some(t) = tables(own).into_iter().find(|t| t.header[0] == "Param") {
            let names: BTreeSet<String> = t.rows.iter().map(|r| code_spans(&r[0])[0].to_string()).collect();
            assert_eq!(names, row.params, "{}: its parameter table", row.op);
        }
        // Its examples: a request for this op with documented parameters only, and a successful
        // response of the documented shape.
        let examples = json_examples(own);
        assert!(
            examples.len() >= 2,
            "{}: a request and a response example",
            row.op
        );
        for example in examples {
            if example.get("op").is_some() {
                let req: RpcRequest = serde_json::from_value(example).expect("a request");
                assert_eq!(req.op, row.op);
                if !req.params.is_null() {
                    assert!(
                        keys(&req.params).is_subset(&row.params),
                        "{}: {}",
                        row.op,
                        req.params
                    );
                }
            } else {
                round_trips::<RpcResponse>(&example);
                assert_eq!(example["ok"], true);
                assert_data_shape(row, &example);
            }
        }
    }
}

/// The defaults and limits the parameter tables give are the ones the engine applies.
#[test]
fn the_parameter_defaults_and_limits_are_the_documented_ones() {
    /// What the dongle is asked for `op` with `params`.
    fn asked(op: &str, params: Value) -> String {
        let mut b = Bridge::with_kitchen();
        let out = b.rpc("d1", op, &params);
        out.iter().find_map(sent_op).expect("a request to the dongle").1
    }
    /// The first number in a table cell's first code span, and the `a..=b` range in the cell.
    fn row_of(op: &str, param: &str) -> Vec<String> {
        let t = table(section(&format!("`{op}`")), "Param");
        t.rows
            .into_iter()
            .find(|r| code_spans(&r[0])[0] == param)
            .unwrap_or_else(|| panic!("{op} has no parameter {param}"))
    }
    let number = |cell: &str| -> u64 { code_spans(cell)[0].parse().expect("a number") };

    // `node_add_ota`: the window's default and its clamp.
    let window = row_of("node_add_ota", "window");
    let default = number(&window[2]);
    let range = code_spans(&window[3])
        .into_iter()
        .find_map(|s| s.split_once("..="))
        .expect("the clamp range");
    let (low, high): (u64, u64) = (range.0.parse().unwrap(), range.1.parse().unwrap());
    assert!(asked("node_add_ota", Value::Null).contains(&format!("window: {default} ")));
    for (given, sent) in [
        (0, low),
        (low, low),
        (high, high),
        (high + 1, high),
        (99_999, high),
    ] {
        let op = asked("node_add_ota", serde_json::json!({ "window": given }));
        assert!(op.contains(&format!("window: {sent} ")), "window {given}: {op}");
    }
    // A value that is not a non-negative integer means the default.
    for odd in [
        serde_json::json!(-5),
        serde_json::json!("60"),
        serde_json::json!(1.5),
    ] {
        let op = asked("node_add_ota", serde_json::json!({ "window": odd }));
        assert!(op.contains(&format!("window: {default} ")), "{op}");
    }

    // `stats_config`: the period's default and its cap.
    let period = row_of("stats_config", "period_ms");
    let default = number(&period[2]);
    let cap: u64 = code_spans(&period[3])
        .into_iter()
        .find_map(|s| s.parse().ok())
        .expect("the cap");
    assert!(asked("stats_config", Value::Null).contains(&format!("channel_period_ms: {default} ")));
    for (given, sent) in [(0, 0), (cap, cap), (cap + 1, cap)] {
        let op = asked("stats_config", serde_json::json!({ "period_ms": given }));
        assert!(op.contains(&format!("channel_period_ms: {sent} ")), "{op}");
    }

    // `node_add`: only `addr` and `key` are required; the rest default as its table says
    // (unnamed, sleeping, both floors 0).
    let defaults: Vec<(String, String)> = ["name", "sleeping", "tx_floor", "rx_floor"]
        .iter()
        .map(|p| {
            (
                p.to_string(),
                code_spans(&row_of("node_add", p)[2])[0].to_string(),
            )
        })
        .collect();
    assert_eq!(
        defaults,
        [
            ("name".to_string(), "\"\"".to_string()),
            ("sleeping".to_string(), "true".to_string()),
            ("tx_floor".to_string(), "0".to_string()),
            ("rx_floor".to_string(), "0".to_string()),
        ]
    );
    let op = asked(
        "node_add",
        serde_json::json!({ "addr": topics::node_hex(B), "key": "0123456789abcdef0001020304050607" }),
    );
    let flags = NODE_FLAG_SLEEPING | NODE_FLAG_UNNAMED;
    for piece in [
        "name: \"\"".to_string(),
        format!("flags: {flags},"),
        "tx_floor: 0,".to_string(),
        "rx_floor: 0 ".to_string(),
    ] {
        assert!(op.contains(&piece), "{piece} not in {op}");
    }
    for required in ["addr", "key"] {
        assert_eq!(row_of("node_add", required)[2], "required");
    }

    // `queue_drop` without `ref` drops the node's whole queue; `queue_list` without `addr` lists
    // every node's.
    let op = asked("queue_drop", serde_json::json!({ "addr": topics::node_hex(A) }));
    assert!(op.contains("item: None"), "{op}");
    assert!(asked("queue_list", Value::Null).contains("node_addr: 0 "));
}

/// The outcomes of `node_add_ota` other than a join, as its section gives them.
#[test]
fn a_pairing_window_without_a_join_answers_joined_null() {
    let row = op_rows().into_iter().find(|r| r.op == "node_add_ota").unwrap();
    // Expired on the dongle: not an error.
    let mut b = Bridge::with_kitchen();
    let out = b.rpc("p1", "node_add_ota", &Value::Null);
    let out = b.answer(&out, mgmt::MGMT_TIMEOUT, &[]);
    b.settle(out);
    let response = b.response("p1");
    assert_eq!(
        response,
        serde_json::json!({ "id": "p1", "ok": true, "data": { "joined": null } })
    );
    assert_data_shape(&row, &response);
    // The last `gateway/pairing` is idle again, without a join.
    let pairing = b.json_since(0, &topics::gateway_pairing(P));
    assert_eq!(pairing.last(), Some(&serde_json::json!({ "state": "idle" })));

    // A second window while one is open, as the section's error list words it.
    let own = unwrapped(section("`node_add_ota`"));
    let mut b = Bridge::with_kitchen();
    b.rpc("p2", "node_add_ota", &Value::Null);
    b.rpc("p3", "node_add_ota", &Value::Null);
    let error = b.response("p3")["error"].as_str().expect("an error").to_string();
    assert!(
        own.contains(&format!("`{error}`")),
        "{error} is not in the section"
    );
    // After a cancel, until the dongle answered the cancelled window.
    b.rpc("p4", "pairing_cancel", &Value::Null);
    b.rpc("p5", "node_add_ota", &Value::Null);
    let other = b.response("p5")["error"].as_str().expect("an error").to_string();
    assert_ne!(other, error);
    assert!(
        own.contains(&format!("`{other}`")),
        "{other} is not in the section"
    );
}

/// The table of errors common to the ops is exactly what the bridge answers: the serial link
/// down, a silent dongle, each refusal code of the dongle, an unknown op.
#[test]
fn the_common_errors_are_the_documented_ones() {
    let t = table(section("4.1 Protocol"), "`error`");
    let documented: BTreeSet<String> = t.rows.iter().map(|r| code_spans(&r[0])[0].to_string()).collect();
    assert_eq!(documented.len(), t.rows.len());

    let error_of = |b: &Bridge, id: &str| -> String {
        let response = b.response(id);
        assert_eq!(response["ok"], false, "{response}");
        assert!(response.get("data").is_none(), "{response}");
        response["error"].as_str().expect("an error text").to_string()
    };
    let mut answered = BTreeSet::new();

    // The bridge has lost the dongle: answered at once.
    let mut b = Bridge::with_kitchen();
    b.handle(Input::SerialDown {
        error: "unplugged".into(),
    });
    b.rpc("e-down", "describe", &Value::Null);
    answered.insert(error_of(&b, "e-down"));

    // The dongle stays silent: failed after the management timeout, not before.
    let mut b = Bridge::with_kitchen();
    b.rpc("e-silent", "describe", &Value::Null);
    for _ in 0..MGMT_TIMEOUT_TICKS - 1 {
        b.handle(Input::Tick);
    }
    assert!(b.json_since(0, &topics::gateway_rsp(P, "e-silent")).is_empty());
    b.handle(Input::Tick);
    answered.insert(error_of(&b, "e-silent"));

    // Each refusal of the dongle.
    for code in [
        mgmt::MGMT_UNSUPPORTED,
        mgmt::MGMT_BAD_ARG,
        mgmt::MGMT_NOT_FOUND,
        mgmt::MGMT_FULL,
        mgmt::MGMT_BUSY,
        mgmt::MGMT_STORAGE,
    ] {
        let mut b = Bridge::with_kitchen();
        let id = format!("e-{code}");
        let out = b.rpc(&id, "stats_config", &Value::Null);
        let out = b.answer(&out, code, &[]);
        b.settle(out);
        let error = error_of(&b, &id);
        assert_eq!(Some(error.as_str()), payload::mgmt_error_str(code));
        answered.insert(error);
    }

    // An op the bridge does not have.
    let mut b = Bridge::with_kitchen();
    b.rpc("e-op", "frobnicate", &Value::Null);
    assert_eq!(error_of(&b, "e-op"), "unknown op \"frobnicate\"");
    answered.insert("unknown op \"<op>\"".to_string());

    assert_eq!(answered, documented);
}

/// A request that breaks the rules of 4.1 is dropped without a response: a bad or repeated id, an
/// oversized or malformed payload, a missing `id` or `op`.
#[test]
fn a_request_that_breaks_the_rules_gets_no_response() {
    let mut b = Bridge::with_kitchen();
    let cmd = topics::gateway_cmd(P);
    let mark = b.pubs.len();
    let long_id = "x".repeat(payload::MAX_REQUEST_ID + 1);
    let oversized = format!(
        r#"{{"id":"big","op":"describe","params":{{"pad":"{}"}}}}"#,
        "x".repeat(MAX_COMMAND_PAYLOAD)
    );
    for bad in [
        r#"{"id":"a/b","op":"describe"}"#.to_string(),
        r#"{"id":"","op":"describe"}"#.to_string(),
        format!(r#"{{"id":"{long_id}","op":"describe"}}"#),
        r#"{"op":"describe"}"#.to_string(),
        r#"{"id":"no-op"}"#.to_string(),
        r#"{"id":"cut","op":"descr"#.to_string(),
        oversized,
    ] {
        let out = b.mqtt(&cmd, bad.as_bytes());
        assert!(out.iter().find_map(sent_op).is_none(), "{bad}");
    }
    assert_eq!(b.pubs.len(), mark, "nothing was published");

    // The longest valid id is answered; its repeat (a QoS 1 redelivery) is not run again.
    let id = "x".repeat(payload::MAX_REQUEST_ID);
    let out = b.rpc(&id, "describe", &Value::Null);
    let out = b.answer(&out, MGMT_OK, &[]);
    b.settle(out);
    let out = b.rpc(&id, "describe", &Value::Null);
    assert!(out.iter().find_map(sent_op).is_none());
    assert_eq!(b.json_since(0, &topics::gateway_rsp(P, &id)).len(), 1);
    // RPC and shell requests share that memory.
    let mark = b.pubs.len();
    let line = format!(r#"{{"id":"{id}","line":"led on"}}"#);
    let out = b.mqtt(&topics::node_shell_req(P, A), line.as_bytes());
    assert!(out.iter().find_map(sent_op).is_none());
    assert_eq!(b.pubs.len(), mark);
    // It holds the last `RECENT_IDS` ids: one more request and the first id is new again.
    for n in 0..RECENT_IDS {
        b.rpc(&format!("fill-{n}"), "frobnicate", &Value::Null);
    }
    let out = b.rpc(&id, "describe", &Value::Null);
    assert!(
        out.iter().find_map(sent_op).is_some(),
        "forgotten after {RECENT_IDS} more"
    );
}

// ---- remote shell -------------------------------------------------------------------------------

/// The table of terminal errors (section 5) is exactly the set of ways the bridge ends a dialog
/// itself; each is one message with `done`, result 1 and no text.
#[test]
fn the_shell_dialog_ends_with_the_documented_errors() {
    let t = table(section("5. Remote shell"), "`error`");
    let documented: BTreeSet<String> = t
        .rows
        .iter()
        .flat_map(|r| code_spans(&r[0]))
        .map(str::to_string)
        .collect();
    let rsp_topic = topics::node_shell_rsp(P, A);

    /// The one terminal error published since `mark`.
    fn ended(b: &Bridge, mark: usize, topic: &str, id: &str) -> String {
        let found = b.json_since(mark, topic);
        assert_eq!(found.len(), 1, "one terminal message for {id}: {found:?}");
        let rsp = &found[0];
        assert_eq!(
            (
                &rsp["id"],
                &rsp["done"],
                &rsp["result"],
                &rsp["text"],
                &rsp["chunk"]
            ),
            (
                &Value::from(id),
                &Value::from(true),
                &Value::from(1),
                &Value::from(""),
                &Value::from(0)
            ),
            "{rsp}"
        );
        rsp["error"].as_str().expect("an error text").to_string()
    }
    let mut ends = BTreeSet::new();

    // The node is not registered.
    let mut b = Bridge::with_kitchen();
    let other = topics::node_shell_rsp(P, 0xFFFF);
    b.mqtt(
        &topics::node_shell_req(P, 0xFFFF),
        br#"{"id":"s1","line":"led on"}"#,
    );
    ends.insert(ended(&b, 0, &other, "s1"));

    // The line does not fit the radio MTU; one that just fits is queued.
    let fits = "x".repeat(radio::RADIO_SHELL_CHUNK);
    let out = b.mqtt(
        &topics::node_shell_req(P, A),
        format!(r#"{{"id":"s2","line":"{fits}"}}"#).as_bytes(),
    );
    assert!(
        out.iter().find_map(sent_op).is_some(),
        "a full-length line is queued"
    );
    let mut b = Bridge::with_kitchen();
    b.mqtt(
        &topics::node_shell_req(P, A),
        format!(r#"{{"id":"s3","line":"{fits}x"}}"#).as_bytes(),
    );
    ends.insert(ended(&b, 0, &rsp_topic, "s3"));

    // The dongle refuses the enqueue (its queue is full).
    let mut b = Bridge::with_kitchen();
    let out = b.mqtt(&topics::node_shell_req(P, A), br#"{"id":"s4","line":"led on"}"#);
    let out = b.answer(&out, mgmt::MGMT_FULL, &[]);
    b.settle(out);
    ends.insert(ended(&b, 0, &rsp_topic, "s4"));

    // The enqueue never reached the dongle, or was not answered.
    let mut b = Bridge::with_kitchen();
    b.handle(Input::SerialDown {
        error: "unplugged".into(),
    });
    b.mqtt(&topics::node_shell_req(P, A), br#"{"id":"s5","line":"led on"}"#);
    ends.insert(ended(&b, 0, &rsp_topic, "s5"));
    let mut b = Bridge::with_kitchen();
    b.mqtt(&topics::node_shell_req(P, A), br#"{"id":"s6","line":"led on"}"#);
    b.ticks(MGMT_TIMEOUT_TICKS);
    ends.insert(ended(&b, 0, &rsp_topic, "s6"));

    // The dongle reports the item expired.
    let mut b = Bridge::with_kitchen();
    b.queue_line("s7", 9, 0);
    b.handle(tx_stat(9, mgmt::TX_EXPIRED, None));
    ends.insert(ended(&b, 0, &rsp_topic, "s7"));

    // No report at all: the backstop, the TTL plus the grace.
    let mut b = Bridge::with_kitchen();
    b.queue_line("s8", 9, 5);
    b.ticks(5 + SHELL_TTL_GRACE_TICKS - 1);
    assert!(
        b.json_since(0, &rsp_topic).is_empty(),
        "not before the TTL plus the grace"
    );
    b.ticks(1);
    ends.insert(ended(&b, 0, &rsp_topic, "s8"));

    // Delivered, then silence.
    let mut b = Bridge::with_kitchen();
    b.queue_line("s9", 9, 0);
    b.handle(tx_stat(9, mgmt::TX_DELIVERED, Some(-50)));
    b.ticks(SHELL_REPLY_TIMEOUT_TICKS - 1);
    assert!(
        b.json_since(0, &rsp_topic).is_empty(),
        "not before the reply timeout"
    );
    b.ticks(1);
    ends.insert(ended(&b, 0, &rsp_topic, "s9"));

    // The dongle rebooted: its RAM queue is gone.
    let mut b = Bridge::with_kitchen();
    b.queue_line("s10", 9, 0);
    let out = b.handle(hello(8));
    b.settle(out);
    ends.insert(ended(&b, 0, &rsp_topic, "s10"));

    // The serial link dropped and came back; the item is no longer on the dongle.
    let mut b = Bridge::with_kitchen();
    b.queue_line("s11", 9, 0);
    b.handle(Input::SerialDown {
        error: "unplugged".into(),
    });
    let out = b.handle(Input::SerialUp);
    b.settle(out);
    ends.insert(ended(&b, 0, &rsp_topic, "s11"));

    // Dropped with `queue_drop`.
    let mut b = Bridge::with_kitchen();
    b.queue_line("s12", 9, 0);
    let params = serde_json::json!({ "addr": topics::node_hex(A), "ref": 9 });
    let out = b.rpc("drop", "queue_drop", &params);
    b.settle(out);
    ends.insert(ended(&b, 0, &rsp_topic, "s12"));

    // The node was removed.
    let mut b = Bridge::with_kitchen();
    b.queue_line("s13", 9, 0);
    let out = b.rpc(
        "rm",
        "node_remove",
        &serde_json::json!({ "addr": topics::node_hex(A) }),
    );
    b.settle(out);
    ends.insert(ended(&b, 0, &rsp_topic, "s13"));

    assert_eq!(ends, documented);
}

/// A dialog as section 5 walks through it: queued (the pending mirror and the node's `queued`),
/// delivered, answered in chunks, a lost chunk flagged `truncated` for the rest of the dialog.
#[test]
fn a_shell_dialog_runs_as_documented() {
    let mut b = Bridge::with_kitchen();
    let pending = topics::node_shell_pending(P, A);
    b.queue_line("d1", 9, 0);
    assert_eq!(
        b.json_since(0, &pending).last(),
        Some(&serde_json::json!([{ "ref": 9, "id": "d1", "line": "led on" }]))
    );
    assert!(
        b.json_since(0, &topics::node_shell_rsp(P, A)).is_empty(),
        "success: no response"
    );
    b.handle(tx_stat(9, mgmt::TX_DELIVERED, Some(-50)));
    // Chunks 0 and 2: chunk 1 was lost on the air.
    let mark = b.pubs.len();
    for (counter, chunk, last, text) in [(10, 0, false, "one "), (11, 2, true, "three")] {
        let msg = NodeMsg::Shell(radio::NodeShellChunk {
            cmd_id: 1,
            result: 0,
            chunk,
            last,
            text,
        });
        let out = b.handle(msg_uplink(A, counter, &msg));
        b.settle(out);
    }
    let chunks = b.json_since(mark, &topics::node_shell_rsp(P, A));
    assert_eq!(
        chunks,
        [
            serde_json::json!({ "id": "d1", "chunk": 0, "text": "one ", "done": false, "result": 0, "truncated": false }),
            serde_json::json!({ "id": "d1", "chunk": 2, "text": "three", "done": true, "result": 0, "truncated": true }),
        ]
    );
    assert_eq!(b.json_since(mark, &pending).last(), Some(&serde_json::json!([])));
}

// ---- properties ---------------------------------------------------------------------------------

/// One property as a row of the kinds tables gives it.
fn assert_prop_documented(prop: &props::Prop, ty: &str, access: &str, default: &str) {
    let name = prop.name;
    let want_access = match prop.access {
        props::Access::State => "state",
        props::Access::Control => "control",
        props::Access::Event => "event",
        props::Access::Param => "setting",
        props::Access::LocalParam => "local setting",
    };
    assert_eq!(access, want_access, "{name}: access");
    match prop.ty {
        props::Type::Bool => assert_eq!(ty, "bool", "{name}: type"),
        props::Type::Enum(names) => assert_eq!(code_spans(ty), names, "{name}: the enumeration"),
        props::Type::Number {
            min,
            max,
            decimals,
            unit,
        } => {
            let rest = ty
                .strip_prefix("number")
                .unwrap_or_else(|| panic!("{name}: documented as {ty}, is a number"));
            let scale = 10f64.powi(decimals.into());
            let (mut doc_unit, mut doc_decimals) = ("", 0u8);
            for part in rest.split(',').map(str::trim) {
                if let Some(count) = part
                    .strip_suffix(" decimals")
                    .or_else(|| part.strip_suffix(" decimal"))
                {
                    doc_decimals = count.parse().expect("a decimals count");
                    continue;
                }
                for word in part.split_whitespace() {
                    if let Some((low, high)) = word.split_once("..") {
                        let (low, high): (f64, f64) = (low.parse().unwrap(), high.parse().unwrap());
                        assert_eq!(
                            ((low * scale).round() as i32, (high * scale).round() as i32),
                            (min, max),
                            "{name}: range"
                        );
                    } else if !word.starts_with('(') {
                        doc_unit = word;
                    }
                }
            }
            assert_eq!(doc_decimals, decimals, "{name}: decimals");
            assert_eq!(doc_unit, unit, "{name}: unit");
        }
    }
    // A setting's default, as JSON shows the value; nothing else has one.
    match prop.ty {
        _ if prop.default.is_empty() => assert_eq!(default, "", "{name}: no default"),
        props::Type::Bool => assert_eq!(
            default,
            if prop.default == "on" { "true" } else { "false" },
            "{name}: default"
        ),
        props::Type::Enum(_) => assert_eq!(default, prop.default, "{name}: default"),
        props::Type::Number { .. } => assert_eq!(
            default.parse::<f64>().ok(),
            prop.default.parse::<f64>().ok(),
            "{name}: default"
        ),
    }
}

/// The property tables of the page are `tower_protocol::props`: the common properties and each
/// kind's own, with their types, ranges, enumerations, access and defaults; the kinds, their
/// firmware names and classes.
#[test]
fn the_property_tables_are_the_protocols() {
    let kinds = section("The kinds");

    let common = table(kinds, "Property");
    assert_eq!(
        common.header,
        ["Property", "Type", "Access", "Default", "Meaning"]
    );
    let mut documented = BTreeSet::new();
    for row in &common.rows {
        // A row may hold several properties of one type (`press`, `release`, ...), with a
        // default each.
        let names = code_spans(&row[0]);
        let defaults = code_spans(&row[3]);
        assert!(defaults.is_empty() || defaults.len() == names.len(), "{row:?}");
        for (i, name) in names.iter().enumerate() {
            let prop = props::COMMON
                .iter()
                .find(|p| p.name == *name)
                .unwrap_or_else(|| panic!("the page has a common property the protocol has not: {name}"));
            assert_prop_documented(prop, &row[1], &row[2], defaults.get(i).copied().unwrap_or(""));
            assert!(documented.insert(prop.name), "{name} is listed twice");
        }
    }
    let all: BTreeSet<&str> = props::COMMON.iter().map(|p| p.name).collect();
    assert_eq!(documented, all, "the common properties");

    let own = table(kinds, "Kind (firmware)");
    assert_eq!(
        own.header,
        [
            "Kind (firmware)",
            "Class",
            "Property",
            "Type",
            "Access",
            "Default",
            "Meaning"
        ]
    );
    let mut seen: Vec<(&props::Kind, BTreeSet<&str>)> = Vec::new();
    for row in &own.rows {
        if !row[0].is_empty() {
            let spans = code_spans(&row[0]);
            let kind = props::kind(spans[0])
                .unwrap_or_else(|| panic!("the page has a kind the protocol has not: {}", spans[0]));
            assert_eq!(spans[1], format!("radio_{}", kind.name), "the firmware name");
            assert_eq!(props::kind_of_firmware(spans[1]), Some(kind));
            let class = match kind.class {
                props::Class::Sleeping => "sleeping",
                props::Class::Listening => "listening",
            };
            assert_eq!(row[1], class, "{}: class", kind.name);
            seen.push((kind, BTreeSet::new()));
        }
        let (kind, listed) = seen.last_mut().expect("the first row names a kind");
        if row[2] == "(the common ones)" {
            assert!(kind.props.is_empty(), "{} has properties of its own", kind.name);
            continue;
        }
        let name = code_spans(&row[2])[0];
        let prop = kind.props.iter().find(|p| p.name == name).unwrap_or_else(|| {
            panic!(
                "{}: the page has a property the protocol has not: {name}",
                kind.name
            )
        });
        let default = code_spans(&row[5]).first().copied().unwrap_or("");
        assert_prop_documented(prop, &row[3], &row[4], default);
        assert!(listed.insert(prop.name), "{}: {name} is listed twice", kind.name);
    }
    for (kind, listed) in &seen {
        let all: BTreeSet<&str> = kind.props.iter().map(|p| p.name).collect();
        assert_eq!(listed, &all, "{}: its own properties", kind.name);
    }
    let documented: BTreeSet<&str> = seen.iter().map(|(k, _)| k.name).collect();
    let all: BTreeSet<&str> = props::KINDS.iter().map(|k| k.name).collect();
    assert_eq!(documented, all, "the kinds");
    assert_eq!(seen.len(), props::KINDS.len(), "a kind is listed twice");
}

/// The access table: what is published where, retained or not, and what a `set` may change.
#[test]
fn the_access_table_is_what_the_bridge_does() {
    let t = table(section("Node properties"), "Access");
    assert_eq!(
        t.header,
        ["Access", "Published on", "Settable with `set`", "Meaning"]
    );
    // One property of each access, all of the `control` kind but the state and the event.
    let cases = [
        ("state", "temperature", props::common::TEMPERATURE, 2150, "21.5"),
        ("control", "relay", props::control::RELAY, 1, "true"),
        ("event", "button", props::common::BUTTON, 2, "\"click\""),
        ("setting", "heartbeat", props::common::HEARTBEAT, 600, "600"),
        ("local setting", "band", props::common::BAND, 1, "\"us915\""),
    ];
    let listed: Vec<&str> = t.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(listed, cases.map(|c| c.0), "the accesses, in the table's order");

    for (row, (access, name, id, value, json)) in t.rows.iter().zip(cases) {
        let mut b = Bridge::with_kitchen();
        let out = b.handle(info_uplink(A, 1, "radio_control"));
        b.settle(out);
        let mark = b.pubs.len();
        let msg = if access == "event" {
            NodeMsg::Event { id, value, count: 1 }
        } else {
            let mut items = heapless::Vec::new();
            items.push((id, value)).unwrap();
            NodeMsg::Report(items)
        };
        b.handle(msg_uplink(A, 2, &msg));
        // Published on the topic the row names, retained as it says, with the JSON value.
        let leaf = code_spans(&row[1])[0].replace("{prop}", name);
        let topic = format!("{}/{leaf}", topics::node(P, A));
        let retained = match row[1].split_once(", ").map(|(_, r)| r) {
            Some("retained") => true,
            Some("not retained") => false,
            other => panic!("{access}: unreadable retain in {other:?}"),
        };
        let found: Vec<&(String, Vec<u8>, bool)> = b.pubs[mark..]
            .iter()
            .filter(|(t, _, _)| t.contains("/state/") || t.contains("/event/"))
            .collect();
        assert_eq!(found.len(), 1, "{access}: {found:?}");
        assert_eq!(
            (found[0].0.as_str(), found[0].2),
            (topic.as_str(), retained),
            "{access}"
        );
        let published: Value = serde_json::from_slice(&found[0].1).unwrap();
        assert_eq!(published["value"].to_string(), json, "{access}");

        // A `set` is queued for the node only where the row says it may be.
        let settable = match row[2].as_str() {
            "yes" => true,
            cell if cell.starts_with("no") => false,
            other => panic!("{access}: unreadable settable cell {other}"),
        };
        let out = b.mqtt(&format!("{}/set/{name}", topics::node(P, A)), json.as_bytes());
        assert_eq!(set_queued(&out, name), settable, "{access}: set/{name}");
    }
}

/// The forms a `set` payload may take, as its section lists them, and what is refused.
#[test]
fn a_set_takes_the_documented_payload_forms() {
    let own = section("`nodes/{addr}/set/{prop}`");
    let queued = |prop: &str, payload: &[u8]| -> bool {
        let mut b = Bridge::with_kitchen();
        let out = b.handle(info_uplink(A, 1, "radio_control"));
        b.settle(out);
        let out = b.mqtt(&format!("{}/set/{prop}", topics::node(P, A)), payload);
        assert!(
            !out.iter().any(|o| matches!(o, Output::Publish { .. })),
            "a set has no response topic"
        );
        set_queued(&out, prop)
    };
    // The page's own spellings: JSON values, an object, plain text, a hex number.
    for form in [
        "`true`",
        "`21.5`",
        "`\"auto\"`",
        "`{\"value\": ...}`",
        "`on`",
        "`auto`",
        "`0xff8000`",
    ] {
        assert!(own.contains(form), "the section no longer shows {form}");
    }
    assert!(queued("relay", b"true"));
    assert!(queued("relay", b"on"));
    assert!(queued("relay", br#"{"value": false}"#));
    assert!(queued("brightness", b"50"));
    assert!(queued("effect", b"fire"));
    assert!(queued("effect", br#""fire""#));
    assert!(queued("color", b"0xff8000"));
    // Refused: out of range, an unknown property, one that is not settable, not a value.
    assert!(!queued("brightness", b"101"));
    assert!(!queued("volume", b"1"));
    assert!(!queued("temperature", b"21.5"));
    assert!(!queued("effect", b"disco"));
    assert!(!queued("relay", b"[true]"));

    // Until the bridge knows the node's kind, it cannot check a set and refuses it (and asks
    // the node to describe itself, which is the one request that goes out).
    let mut b = Bridge::with_kitchen();
    let out = b.mqtt(&format!("{}/set/relay", topics::node(P, A)), b"true");
    let lines = logs(&out);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("refusing set relay") && lines[0].contains("kind is not known yet"),
        "{lines:?}"
    );
    // The examples of the section name real properties of real kinds.
    for line in own.lines().filter(|l| l.starts_with("mosquitto_pub")) {
        let prop = line
            .split("/set/")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("a set example");
        assert!(
            props::KINDS.iter().any(|k| k
                .prop_named(prop)
                .is_some_and(|p| matches!(p.access, props::Access::Control | props::Access::Param))),
            "{prop} is not a settable property of any kind"
        );
    }
}

// ---- the numbers in the prose -------------------------------------------------------------------

/// The limits and periods the page states in running text are the constants the bridge uses. A
/// sentence reworded on the page needs its phrase updated here; a constant changed in the code
/// needs the page updated.
#[test]
fn the_numbers_in_the_prose_are_the_constants() {
    let page = unwrapped(DOC);
    let target = crate::gateway::tls::Target {
        host: "localhost".into(),
        port: 1883,
        tls: None,
    };
    let keep_alive = crate::gateway::mqtt::options("contract".into(), &target, None)
        .keep_alive()
        .as_secs();
    let facts = [
        format!("currently **{}**", payload::SCHEMA),
        format!("Version of this API (section 1), currently `{}`", payload::SCHEMA),
        format!("1 to {} characters of `[A-Za-z0-9_.-]`", payload::MAX_REQUEST_ID),
        format!("remembers the last {RECENT_IDS} ids"),
        format!("Payloads over {MAX_COMMAND_PAYLOAD} bytes"),
        format!("at most {} bytes (the radio MTU)", radio::RADIO_SHELL_CHUNK),
        format!(
            "`line too long for the radio MTU (max {} bytes)`",
            radio::RADIO_SHELL_CHUNK
        ),
        format!("`0` or absent = {DEFAULT_QUEUE_TTL}"),
        format!("refreshed every {STATS_EVERY_TICKS} s"),
        format!("unanswered after about {MGMT_TIMEOUT_TICKS} s"),
        format!("no reply chunk for {SHELL_REPLY_TIMEOUT_TICKS} s"),
        format!("`ttl` plus {SHELL_TTL_GRACE_TICKS} s without any report"),
        format!("the bridge waits the window plus {PAIRING_SLACK_S} s"),
        format!("No answer within the window plus {PAIRING_SLACK_S} s"),
        format!("The answer lapses after {TIME_TTL_S} s"),
        format!("Friendly name, at most {} bytes", mgmt::MAX_NODE_NAME),
        format!("1 to {} bytes of UTF-8", mgmt::MAX_NODE_NAME),
        format!("(`name must be 1..={} bytes`)", mgmt::MAX_NODE_NAME),
        format!("caps packets at {} KiB", crate::gateway::mqtt::MAX_PAYLOAD / 1024),
        format!("one and a half keep-alive periods; the bridge uses {keep_alive} s"),
    ];
    for fact in facts {
        assert!(page.contains(&fact), "the page no longer says: {fact}");
    }
}

/// The last will the bridge connects with is the one section 1 spells out: on `gateway/status`,
/// retained, QoS 1, the offline payload.
#[test]
fn the_last_will_is_the_documented_one() {
    use crate::gateway::{mqtt, tls};
    let params = mqtt::MqttParams {
        target: tls::Target {
            host: "localhost".into(),
            port: 1883,
            tls: None,
        },
        credentials: None,
        client_id: "tower-gateway-0x00000001".into(),
    };
    // Built, never polled: nothing is dialed.
    let (_client, connection) = mqtt::client(&params, P);
    let will = connection
        .eventloop
        .mqtt_options
        .last_will()
        .expect("the bridge connects with a last will");
    let sent: Value = serde_json::from_slice(&will.message).expect("JSON");
    assert_eq!(sent, payload::Status::offline());

    let page = unwrapped(DOC);
    let paragraph = page
        .split_once("**Last will.**")
        .and_then(|(_, rest)| rest.split_once("**Schema version.**"))
        .expect("the last will paragraph")
        .0;
    let spans = code_spans(paragraph);
    assert_eq!(
        format!("{P}{}", spans[0]),
        will.topic,
        "the topic the paragraph names first"
    );
    assert!(paragraph.contains("retained, QoS 1, payload"), "{paragraph}");
    assert!(will.retain);
    assert_eq!(will.qos, rumqttc::QoS::AtLeastOnce);
    let payloads: Vec<Value> = spans
        .into_iter()
        .filter(|s| s.starts_with('{'))
        .map(|s| serde_json::from_str(s).expect("JSON"))
        .collect();
    assert_eq!(payloads, [sent]);
    // The topic table marks the row, and it is a retained one.
    let row = table(section("2. Topics"), "Topic")
        .rows
        .into_iter()
        .find(|r| r[1].contains("last will"))
        .expect("the row with the last will");
    assert_eq!(code_spans(&row[0])[0], "gateway/status");
    assert_eq!(row[2], "yes");
}

/// The name rules of `node_rename` (also `node_add`'s): each refusal is worded as its section
/// quotes it.
#[test]
fn the_name_rules_are_worded_as_documented() {
    let own = unwrapped(section("`node_rename`"));
    let refused = |name: &str| -> String {
        let mut b = Bridge::with_kitchen();
        // A second node, so a name can be taken.
        let mut listing = node_entry_record(A, "kitchen", NODE_FLAG_SLEEPING);
        listing.extend(node_entry_record(B, "hall", 0));
        let mut out = Vec::new();
        b.e.issue(&mut out, OpKind::NodeList, MgmtOp::NodeList, None);
        let out = b.answer(&out, MGMT_OK, &listing);
        b.settle(out);
        let params = serde_json::json!({ "addr": topics::node_hex(A), "name": name });
        b.rpc("n1", "node_rename", &params);
        b.response("n1")["error"].as_str().expect("refused").to_string()
    };
    for name in ["", "seventeen-bytes-x", "bell\u{7}", "0x0000ab12", "hall"] {
        let error = refused(name);
        let quoted = if name == "hall" {
            error
                .replace("hall", "<name>")
                .replace(&topics::node_hex(B), "<addr>")
        } else {
            error.clone()
        };
        assert!(
            own.contains(&format!("`{quoted}`")),
            "{name:?}: {error} is not in the section"
        );
    }
    // The longest name is taken.
    let mut b = Bridge::with_kitchen();
    let longest = "x".repeat(mgmt::MAX_NODE_NAME);
    let params = serde_json::json!({ "addr": topics::node_hex(A), "name": longest });
    let out = b.rpc("n2", "node_rename", &params);
    assert!(out.iter().find_map(sent_op).is_some());
    // And the other errors the section lists.
    let mut b = Bridge::with_kitchen();
    b.rpc(
        "n3",
        "node_rename",
        &serde_json::json!({ "addr": "nobody", "name": "x" }),
    );
    b.rpc(
        "n4",
        "node_rename",
        &serde_json::json!({ "addr": topics::node_hex(A) }),
    );
    for id in ["n3", "n4"] {
        let error = b.response(id)["error"].as_str().expect("refused").to_string();
        assert!(
            own.contains(&format!("`{error}`")),
            "{error} is not in the section"
        );
    }
}
