//! The console page's terminal: xterm.js on a WebSocket to the shared
//! serial console. Only the browser runs this.

use std::cell::RefCell;
use std::rc::Rc;

use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// xterm.js's `Terminal`, which its script defines.
    type Terminal;

    #[wasm_bindgen(constructor)]
    fn new(options: &JsValue) -> Terminal;

    #[wasm_bindgen(method)]
    fn open(this: &Terminal, parent: &web_sys::HtmlElement);

    #[wasm_bindgen(method, js_name = write)]
    fn write_bytes(this: &Terminal, data: &js_sys::Uint8Array);

    #[wasm_bindgen(method, js_name = onData)]
    fn on_data(this: &Terminal, listener: &Closure<dyn FnMut(String)>);

    #[wasm_bindgen(method)]
    fn resize(this: &Terminal, cols: u32, rows: u32);

    #[wasm_bindgen(method)]
    fn focus(this: &Terminal);

    #[wasm_bindgen(method)]
    fn reset(this: &Terminal);
}

/// What the server says about the console besides its bytes.
#[derive(Deserialize)]
struct Status {
    connected: bool,
    viewers: Vec<String>,
}

/// Opens the terminal in `#term` once xterm.js has loaded, and keeps it
/// connected to the workspace's console.
pub fn attach(name: &str, me: &str, viewers: RwSignal<String>) {
    let has_xterm = js_sys::Reflect::has(&window(), &"Terminal".into()).unwrap_or(false);
    if !has_xterm {
        let (name, me) = (name.to_owned(), me.to_owned());
        set_timeout(move || attach(&name, &me, viewers), std::time::Duration::from_millis(50));
        return;
    }
    let Some(el) = document().get_element_by_id("term").and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()) else {
        return;
    };
    let mono = window()
        .get_computed_style(&document().document_element().expect("a document element"))
        .ok()
        .flatten()
        .and_then(|s| s.get_property_value("--mono").ok())
        .unwrap_or_else(|| "monospace".into());
    let options = serde_wasm_options(&mono);
    let term = Rc::new(Terminal::new(&options));
    term.open(&el);
    fit(&term, &el);
    {
        let (term, el) = (term.clone(), el.clone());
        let resize = window_event_listener(leptos::ev::resize, move |_| fit(&term, &el));
        on_cleanup(move || resize.remove());
    }
    let socket: Rc<RefCell<Option<web_sys::WebSocket>>> = Rc::default();
    // Leaving the page closes the console and stops reconnecting to it.
    let closed = Rc::new(std::cell::Cell::new(false));
    {
        let (socket, closed) =
            (send_wrapper::SendWrapper::new(socket.clone()), send_wrapper::SendWrapper::new(closed.clone()));
        on_cleanup(move || {
            closed.set(true);
            if let Some(ws) = socket.borrow_mut().take() {
                let _ = ws.close();
            }
        });
    }
    {
        let socket = socket.clone();
        let on_data = Closure::<dyn FnMut(String)>::new(move |data: String| {
            if let Some(ws) = socket.borrow().as_ref()
                && ws.ready_state() == web_sys::WebSocket::OPEN
            {
                let _ = ws.send_with_u8_array(data.as_bytes());
            }
        });
        term.on_data(&on_data);
        on_data.forget();
    }
    connect(&Conn { name: name.to_owned(), me: me.to_owned(), term: term.clone(), socket, closed, viewers }, false);
    term.focus();
}

fn serde_wasm_options(font: &str) -> JsValue {
    let options = js_sys::Object::new();
    let set = |k: &str, v: JsValue| {
        let _ = js_sys::Reflect::set(&options, &k.into(), &v);
    };
    set("fontFamily", font.into());
    set("fontSize", 13.into());
    set("cursorBlink", true.into());
    set("scrollback", 5000.into());
    let theme = js_sys::Object::new();
    for (k, v) in [
        ("background", "#0a0c0b"),
        ("foreground", "#d5dbd7"),
        ("cursor", "#f2eee5"),
        ("selectionBackground", "#3a4540"),
    ] {
        let _ = js_sys::Reflect::set(&theme, &k.into(), &v.into());
    }
    set("theme", theme.into());
    options.into()
}

/// A serial console has no size of its own: the terminal fills its space.
fn fit(term: &Terminal, el: &web_sys::HtmlElement) {
    let cell = ["_core", "_renderService", "dimensions", "css", "cell"]
        .iter()
        .try_fold(JsValue::from(term), |v, k| js_sys::Reflect::get(&v, &(*k).into()).ok().filter(JsValue::is_object));
    let dim = |k: &str, default: f64| {
        cell.as_ref()
            .and_then(|c| js_sys::Reflect::get(c, &k.into()).ok())
            .and_then(|v| v.as_f64())
            .filter(|v| *v > 0.0)
            .unwrap_or(default)
    };
    let (w, h) = (dim("width", 7.8), dim("height", 17.0));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cols = ((f64::from(el.client_width()) - 14.0) / w).floor().max(20.0) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rows = ((f64::from(el.client_height()) - 20.0) / h).floor().max(5.0) as u32;
    term.resize(cols, rows);
}

/// What a connection to the console needs, kept for reconnecting.
#[derive(Clone)]
struct Conn {
    name: String,
    me: String,
    term: Rc<Terminal>,
    socket: Rc<RefCell<Option<web_sys::WebSocket>>>,
    closed: Rc<std::cell::Cell<bool>>,
    viewers: RwSignal<String>,
}

/// Connects to the shared console. A reconnection clears the terminal
/// first: the server sends what it kept again.
fn connect(c: &Conn, again: bool) {
    if c.closed.get() {
        return;
    }
    let loc = window().location();
    let scheme = if loc.protocol().as_deref() == Ok("https:") { "wss" } else { "ws" };
    let host = loc.host().unwrap_or_default();
    let Ok(ws) = web_sys::WebSocket::new(&format!("{scheme}://{host}/w/{}/console/socket", c.name)) else { return };
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
    if again {
        let term = c.term.clone();
        let on_open = Closure::<dyn FnMut()>::new(move || term.reset());
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();
    }
    {
        let (term, me, viewers) = (c.term.clone(), c.me.clone(), c.viewers);
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            let data = e.data();
            if let Some(text) = data.as_string() {
                if let Ok(st) = serde_json::from_str::<Status>(&text) {
                    let mut others: Vec<&String> = st.viewers.iter().filter(|v| **v != me).collect();
                    others.dedup();
                    let who = if others.is_empty() {
                        "you".to_owned()
                    } else {
                        format!("you and {}", others.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
                    };
                    let down = if st.connected { "" } else { "VM not running · " };
                    viewers.set(format!("{down}{} connected · {who}", st.viewers.len()));
                }
            } else if let Ok(buf) = data.dyn_into::<js_sys::ArrayBuffer>() {
                term.write_bytes(&js_sys::Uint8Array::new(&buf));
            }
        });
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
    }
    {
        let c2 = c.clone();
        let on_close = Closure::<dyn FnMut()>::new(move || {
            if c2.closed.get() {
                return;
            }
            c2.viewers.set("Disconnected · reconnecting…".into());
            let c3 = c2.clone();
            set_timeout(move || connect(&c3, true), std::time::Duration::from_secs(2));
        });
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        on_close.forget();
    }
    *c.socket.borrow_mut() = Some(ws);
}
