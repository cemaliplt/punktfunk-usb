//! The small corner of XML property lists usbmuxd speaks. Not a general plist library: it writes
//! flat request dictionaries and reads response trees (dict, array, integer, string, true/false,
//! real, data, date). Binary plists are refused with a clear error.

use std::io;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Dict(Vec<(String, Value)>),
    Array(Vec<Value>),
    Int(i64),
    Str(String),
    Bool(bool),
    /// real, data, date: kept as their text, nobody here needs them.
    Other(String),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(d) => d.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        if let Value::Int(i) = self {
            Some(*i)
        } else {
            None
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Value::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_array(&self) -> Option<&[Value]> {
        if let Value::Array(a) = self {
            Some(a)
        } else {
            None
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unesc(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Serialises a flat dictionary of strings and integers.
pub fn write_dict(items: &[(&str, Value)]) -> Vec<u8> {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    for (k, v) in items {
        s.push_str(&format!("\t<key>{}</key>\n", esc(k)));
        match v {
            Value::Int(i) => s.push_str(&format!("\t<integer>{i}</integer>\n")),
            Value::Str(t) => s.push_str(&format!("\t<string>{}</string>\n", esc(t))),
            Value::Bool(b) => s.push_str(if *b { "\t<true/>\n" } else { "\t<false/>\n" }),
            _ => unreachable!("write_dict takes flat values only"),
        }
    }
    s.push_str("</dict>\n</plist>\n");
    s.into_bytes()
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("plist: {msg}"))
}

struct P<'a> {
    s: &'a str,
    i: usize,
}

enum Tok<'a> {
    Open(&'a str),
    Close(&'a str),
    SelfClosed(&'a str),
}

impl<'a> P<'a> {
    fn skip_decl(&mut self) {
        loop {
            let rest = &self.s[self.i..];
            let t = rest.trim_start();
            self.i += rest.len() - t.len();
            let rest = &self.s[self.i..];
            if rest.starts_with("<?") {
                match rest.find("?>") {
                    Some(e) => self.i += e + 2,
                    None => return,
                }
            } else if rest.starts_with("<!") {
                match rest.find('>') {
                    Some(e) => self.i += e + 1,
                    None => return,
                }
            } else {
                return;
            }
        }
    }

    fn tag(&mut self) -> io::Result<Tok<'a>> {
        let rest = &self.s[self.i..];
        let rest_t = rest.trim_start();
        self.i += rest.len() - rest_t.len();
        let rest = &self.s[self.i..];
        if !rest.starts_with('<') {
            return Err(bad("expected a tag"));
        }
        let e = rest.find('>').ok_or_else(|| bad("unterminated tag"))?;
        let inner = &rest[1..e];
        self.i += e + 1;
        if let Some(n) = inner.strip_prefix('/') {
            Ok(Tok::Close(n.trim()))
        } else if let Some(n) = inner.strip_suffix('/') {
            Ok(Tok::SelfClosed(n.split_whitespace().next().unwrap_or("")))
        } else {
            Ok(Tok::Open(inner.split_whitespace().next().unwrap_or("")))
        }
    }

    /// Text up to the next `<`.
    fn text(&mut self) -> &'a str {
        let rest = &self.s[self.i..];
        let e = rest.find('<').unwrap_or(rest.len());
        self.i += e;
        &rest[..e]
    }

    fn value_after_open(&mut self, name: &str) -> io::Result<Value> {
        match name {
            "dict" => {
                let mut items = Vec::new();
                loop {
                    match self.tag()? {
                        Tok::Close("dict") => return Ok(Value::Dict(items)),
                        Tok::Open("key") => {
                            let k = unesc(self.text());
                            match self.tag()? {
                                Tok::Close("key") => {}
                                _ => return Err(bad("key not closed")),
                            }
                            let v = self.value()?;
                            items.push((k, v));
                        }
                        _ => return Err(bad("unexpected tag in dict")),
                    }
                }
            }
            "array" => {
                let mut items = Vec::new();
                loop {
                    let save = self.i;
                    match self.tag()? {
                        Tok::Close("array") => return Ok(Value::Array(items)),
                        Tok::Open(n) => items.push(self.value_after_open(n)?),
                        Tok::SelfClosed(n) => items.push(Self::selfclosed(n)?),
                        Tok::Close(_) => {
                            self.i = save;
                            return Err(bad("unexpected close in array"));
                        }
                    }
                }
            }
            "integer" | "string" | "real" | "data" | "date" => {
                let t = unesc(self.text());
                match self.tag()? {
                    Tok::Close(c) if c == name => {}
                    _ => return Err(bad("scalar not closed")),
                }
                Ok(match name {
                    "integer" => Value::Int(t.trim().parse().map_err(|_| bad("bad integer"))?),
                    "string" => Value::Str(t),
                    _ => Value::Other(t),
                })
            }
            other => Err(bad(&format!("unknown element <{other}>"))),
        }
    }

    fn selfclosed(name: &str) -> io::Result<Value> {
        match name {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "dict" => Ok(Value::Dict(Vec::new())),
            "array" => Ok(Value::Array(Vec::new())),
            "string" => Ok(Value::Str(String::new())),
            other => Err(bad(&format!("unknown empty element <{other}/>"))),
        }
    }

    fn value(&mut self) -> io::Result<Value> {
        match self.tag()? {
            Tok::Open(n) => self.value_after_open(n),
            Tok::SelfClosed(n) => Self::selfclosed(n),
            Tok::Close(_) => Err(bad("value expected")),
        }
    }
}

pub fn parse(bytes: &[u8]) -> io::Result<Value> {
    if bytes.starts_with(b"bplist00") {
        return Err(bad(
            "binary plist (usbmuxd answered in a format we do not read)",
        ));
    }
    let s = std::str::from_utf8(bytes).map_err(|_| bad("not UTF-8"))?;
    let mut p = P { s, i: 0 };
    p.skip_decl();
    match p.tag()? {
        Tok::Open("plist") => {}
        _ => return Err(bad("no <plist>")),
    }
    p.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>DeviceList</key>
	<array>
		<dict>
			<key>DeviceID</key>
			<integer>3</integer>
			<key>MessageType</key>
			<string>Attached</string>
			<key>Properties</key>
			<dict>
				<key>ConnectionSpeed</key>
				<integer>480000000</integer>
				<key>ConnectionType</key>
				<string>USB</string>
				<key>DeviceID</key>
				<integer>3</integer>
				<key>SerialNumber</key>
				<string>00008027-001A&amp;B</string>
				<key>UsesEscapes</key>
				<true/>
			</dict>
		</dict>
		<dict>
			<key>DeviceID</key>
			<integer>4</integer>
			<key>Properties</key>
			<dict><key>ConnectionType</key><string>Network</string></dict>
		</dict>
	</array>
</dict>
</plist>
"#;

    #[test]
    fn parses_a_listdevices_reply() {
        let v = parse(LIST.as_bytes()).unwrap();
        let devs = v.get("DeviceList").unwrap().as_array().unwrap();
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].get("DeviceID").unwrap().as_int(), Some(3));
        let p = devs[0].get("Properties").unwrap();
        assert_eq!(p.get("ConnectionType").unwrap().as_str(), Some("USB"));
        assert_eq!(
            p.get("SerialNumber").unwrap().as_str(),
            Some("00008027-001A&B")
        );
        assert_eq!(p.get("UsesEscapes"), Some(&Value::Bool(true)));
        assert_eq!(
            devs[1]
                .get("Properties")
                .unwrap()
                .get("ConnectionType")
                .unwrap()
                .as_str(),
            Some("Network")
        );
    }

    #[test]
    fn parses_a_result_reply() {
        let r = b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>MessageType</key><string>Result</string><key>Number</key><integer>3</integer></dict></plist>";
        let v = parse(r).unwrap();
        assert_eq!(v.get("Number").unwrap().as_int(), Some(3));
    }

    #[test]
    fn written_requests_parse_back() {
        let b = write_dict(&[
            ("MessageType", Value::Str("Connect".into())),
            ("DeviceID", Value::Int(7)),
            ("PortNumber", Value::Int(0x0B25)),
            ("Note", Value::Str("a<b&c".into())),
        ]);
        let v = parse(&b).unwrap();
        assert_eq!(v.get("DeviceID").unwrap().as_int(), Some(7));
        assert_eq!(v.get("PortNumber").unwrap().as_int(), Some(0x0B25));
        assert_eq!(v.get("Note").unwrap().as_str(), Some("a<b&c"));
    }

    #[test]
    fn rejects_binary_and_garbage() {
        assert!(parse(b"bplist00....").is_err());
        assert!(parse(b"hello").is_err());
        assert!(parse(b"<plist><dict><key>a</key>").is_err());
    }
}
