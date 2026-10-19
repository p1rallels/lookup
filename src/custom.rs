//! The custom messages lookup reads closely: msg's agent-to-agent messages
//! and relay's context resets, and how human output names them.

use serde::Serialize;
use serde_json::{Number, Value};

use crate::naming::Names;

pub const MSG: &str = "msg";
pub const RELAY: &str = "relay";

/// A session on msg's side of a message: its id, and the name and folder it
/// went by.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Party {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// What msg's Message object says besides its text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Msg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub from: Party,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// `ended` (a child's last words) or `died`; none for a normal message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// An `ended` from a sub that went away mid-run, or after an aborted run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
    /// A human sent it (`/msg` or the msg cli), not an agent.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub by_user: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// Relay's details on a nudge, reset or loop message, or on the compaction
/// a reset wrote (which has no `kind`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Relay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
}

/// msg's Message object, when it names a sender.
pub fn parse_msg(message: &Value) -> Option<Msg> {
    let from = message.get("from")?;
    Some(Msg {
        id: text_field(message, "id"),
        from: Party {
            id: from.get("id")?.as_str()?.to_owned(),
            name: text_field(from, "name"),
            cwd: text_field(from, "cwd"),
        },
        to: text_field(message, "to"),
        kind: text_field(message, "kind"),
        stopped: message.get("stopped").and_then(Value::as_bool) == Some(true),
        by_user: message.get("byUser").and_then(Value::as_bool) == Some(true),
        reply_to: text_field(message, "replyTo"),
    })
}

pub fn parse_relay(details: &Value) -> Relay {
    Relay {
        kind: text_field(details, "kind"),
        percent: match details.get("percent") {
            Some(Value::Number(percent)) => Some(percent.clone()),
            _ => None,
        },
        n: details.get("n").and_then(Value::as_u64),
        note: text_field(details, "note"),
        goal: text_field(details, "goal"),
    }
}

/// A compaction's relay details: set on the retain-none compaction relay
/// writes for each reset.
pub fn compaction_relay(event: &Value) -> Option<Relay> {
    event
        .pointer("/details/relay")
        .filter(|relay| relay.is_object())
        .map(parse_relay)
}

/// Messages a msg tool's result carries: the one msg_wait took, the one
/// msg_send sent and the reply it waited for.
pub fn tool_messages(message: &Value) -> Vec<&Value> {
    let pointers: &[&str] = match message.get("toolName").and_then(Value::as_str) {
        Some("msg_wait") => &["/details/message"],
        Some("msg_send") => &["/details/message", "/details/reply"],
        _ => &[],
    };
    pointers
        .iter()
        .filter_map(|pointer| message.pointer(pointer))
        .filter(|found| found.is_object())
        .collect()
}

/// A custom message's text: a string, or its text parts one per line.
pub fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `msg scout → you`, `msg csv ended`, `msg user (via main) → you`: from the
/// shown session's side, which is `you`. The human behind a byUser message
/// is `user`, as in lookup's `user` kind, sending through some session.
pub fn msg_label(msg: &Msg, session: &str, names: &Names) -> String {
    let from = party(&msg.from.id, msg.from.name.as_deref(), session, names);
    let sender = match (msg.by_user, from) {
        (true, "you") => "user".to_owned(),
        (true, via) => format!("user (via {via})"),
        (false, from) => from.to_owned(),
    };
    match (exit_kind(msg), msg.to.as_deref()) {
        (Some(kind), _) => format!("msg {sender} {kind}"),
        (None, Some(to)) => format!("msg {sender} → {}", party(to, None, session, names)),
        (None, None) => format!("msg {sender}"),
    }
}

/// How a sub's exit reads: `stopped` for an `ended` that didn't finish its
/// run, else msg's own kind (`ended`, `died`).
pub fn exit_kind(msg: &Msg) -> Option<&str> {
    match (msg.kind.as_deref(), msg.stopped) {
        (Some("ended"), true) => Some("stopped"),
        (kind, _) => kind,
    }
}

/// `relay 45% nudge`, `relay 48% reset #2`.
pub fn relay_label(relay: &Relay) -> String {
    [Some(RELAY.to_owned()), percent(relay), relay.kind.clone()]
        .into_iter()
        .flatten()
        .chain(relay.n.map(|n| format!("#{n}")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// How a relay compaction reads: `relay reset #2 at 48%`.
pub fn reset_label(relay: &Relay) -> String {
    let number = relay.n.map(|n| format!(" #{n}")).unwrap_or_default();
    let at = percent(relay)
        .map(|percent| format!(" at {percent}"))
        .unwrap_or_default();
    format!("relay reset{number}{at}")
}

/// A party from the shown session's side: `you`, else its name, else its id
/// as lookup prints ids.
fn party<'a>(id: &'a str, name: Option<&'a str>, session: &str, names: &'a Names) -> &'a str {
    match (id.eq_ignore_ascii_case(session), name) {
        (true, _) => "you",
        (false, Some(name)) => name,
        (false, None) => names.id(id),
    }
}

fn percent(relay: &Relay) -> Option<String> {
    relay
        .percent
        .as_ref()
        .and_then(Number::as_f64)
        .map(|percent| format!("{percent:.0}%"))
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{
        compaction_relay, content_text, msg_label, parse_msg, parse_relay, relay_label,
        reset_label, tool_messages,
    };
    use crate::naming::Names;
    use serde_json::json;

    const ME: &str = "01a15300-0000-7000-8000-00000000000a";

    fn message(extra: serde_json::Value) -> serde_json::Value {
        let mut message = json!({
            "v": 1, "id": "0mv9-aa", "ts": 1, "to": ME, "text": "hi", "delivery": "steer",
            "from": {"id": "01a15301-0000-7000-8000-00000000000c", "name": "scout"}
        });
        message
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        message
    }

    fn label(extra: serde_json::Value) -> String {
        msg_label(&parse_msg(&message(extra)).unwrap(), ME, &Names::default())
    }

    #[test]
    fn labels_messages_from_the_shown_sessions_side() {
        assert_eq!(label(json!({})), "msg scout → you");
        assert_eq!(label(json!({"kind": "ended"})), "msg scout ended");
        assert_eq!(
            label(json!({"kind": "ended", "stopped": true})),
            "msg scout stopped"
        );
        assert_eq!(label(json!({"kind": "died"})), "msg scout died");
        assert_eq!(
            label(json!({"from": {"id": "01a152a0-f00d-72aa-a896-d904c458282b"}, "byUser": true})),
            "msg user (via 01a152a0) → you"
        );
        assert_eq!(
            label(json!({"from": {"id": ME, "name": "main"}, "to": "01a15301-0000"})),
            "msg you → 01a15301"
        );
        assert_eq!(
            label(json!({"from": {"id": ME}, "to": "01a15301-0000", "byUser": true})),
            "msg user → 01a15301"
        );
    }

    #[test]
    fn reads_the_whole_message_object() {
        let msg = parse_msg(&message(json!({"replyTo": "0mv8-bb", "byUser": true}))).unwrap();
        assert_eq!(msg.id.as_deref(), Some("0mv9-aa"));
        assert_eq!(msg.from.name.as_deref(), Some("scout"));
        assert_eq!(msg.to.as_deref(), Some(ME));
        assert_eq!(msg.reply_to.as_deref(), Some("0mv8-bb"));
        assert!(msg.by_user);
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({"id": "0mv9-aa", "from": {"id": "01a15301-0000-7000-8000-00000000000c",
                "name": "scout"}, "to": ME, "by_user": true, "reply_to": "0mv8-bb"})
        );
        assert!(parse_msg(&json!({"text": "no sender"})).is_none());
    }

    #[test]
    fn labels_relay_messages_and_resets() {
        let nudge = parse_relay(&json!({"kind": "nudge", "percent": 45.4}));
        assert_eq!(relay_label(&nudge), "relay 45% nudge");
        let reset = parse_relay(&json!({"kind": "reset", "percent": 48, "n": 2}));
        assert_eq!(relay_label(&reset), "relay 48% reset #2");
        assert_eq!(relay_label(&parse_relay(&json!({}))), "relay");
        let compaction = json!({"type": "compaction", "fromHook": true,
            "details": {"relay": {"n": 2, "percent": 48, "note": "rerun db"}}});
        let relay = compaction_relay(&compaction).unwrap();
        assert_eq!(reset_label(&relay), "relay reset #2 at 48%");
        assert_eq!(relay.note.as_deref(), Some("rerun db"));
        assert_eq!(
            serde_json::to_value(&relay).unwrap(),
            json!({"percent": 48, "n": 2, "note": "rerun db"})
        );
        assert!(compaction_relay(&json!({"details": {"readFiles": []}})).is_none());
    }

    #[test]
    fn finds_messages_in_msg_tool_results() {
        let sent = message(json!({}));
        let reply = message(json!({"text": "no"}));
        let send = json!({"role": "toolResult", "toolName": "msg_send",
            "details": {"message": sent, "reply": reply, "outcome": "message"}});
        assert_eq!(tool_messages(&send), [&sent, &reply]);
        let wait = json!({"role": "toolResult", "toolName": "msg_wait",
            "details": {"outcome": "timeout"}});
        assert!(tool_messages(&wait).is_empty());
        let bash = json!({"role": "toolResult", "toolName": "bash", "details": {"message": sent}});
        assert!(tool_messages(&bash).is_empty());
    }

    #[test]
    fn reads_string_and_part_content() {
        assert_eq!(content_text(Some(&json!("framed"))), "framed");
        assert_eq!(
            content_text(Some(&json!([{"type": "text", "text": "a"},
                {"type": "image", "data": "…"}, {"type": "text", "text": "b"}]))),
            "a\nb"
        );
        assert_eq!(content_text(None), "");
    }
}
