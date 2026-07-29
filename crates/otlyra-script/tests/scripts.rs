//! What a page's script may do, what it may not, and what happens when it will
//! not stop.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otlyra_html::HtmlParser;
use otlyra_script::{PageScripts, ScriptHost};
use otter_runtime::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle};

/// A console that keeps what it was told, so a test can read it back.
#[derive(Debug, Default)]
struct Captured {
    lines: Mutex<Vec<String>>,
}

impl ConsoleSink for Captured {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        self.lines
            .lock()
            .expect("no test panics while holding this")
            .push(format!("{level:?}: {}", fields.join(" ")));
    }
}

fn capturing() -> (ScriptHost, Arc<Captured>) {
    let sink = Arc::new(Captured::default());
    let handle: ConsoleSinkHandle = Arc::clone(&sink) as ConsoleSinkHandle;
    (
        ScriptHost::with_console(handle).expect("the isolate builds"),
        sink,
    )
}

fn lines(sink: &Captured) -> Vec<String> {
    sink.lines
        .lock()
        .expect("no test panics while holding this")
        .clone()
}

#[test]
fn a_script_runs_and_its_console_output_arrives() {
    let (mut host, sink) = capturing();
    host.run_classic_script("console.log('hi', 1 + 1)", "test.html")
        .expect("the script runs");
    assert_eq!(lines(&sink), vec!["Log: hi 2".to_owned()]);
}

#[test]
fn two_scripts_share_one_global() {
    let (mut host, sink) = capturing();
    host.run_classic_script("var shared = 41", "one")
        .expect("the first script runs");
    host.run_classic_script("console.log(shared + 1)", "two")
        .expect("the second script runs");
    assert_eq!(lines(&sink), vec!["Log: 42".to_owned()]);
}

#[test]
fn a_syntax_error_is_reported_and_the_isolate_survives() {
    let (mut host, sink) = capturing();
    let error = host
        .run_classic_script("function (", "broken.html")
        .expect_err("a syntax error is an error");
    assert_eq!(error.specifier, "broken.html");
    assert!(!error.interrupted, "nothing interrupted it: {error}");

    host.run_classic_script("console.log('still here')", "after")
        .expect("the isolate is still usable");
    assert_eq!(lines(&sink), vec!["Log: still here".to_owned()]);
}

#[test]
fn a_thrown_exception_is_reported_and_the_isolate_survives() {
    let (mut host, sink) = capturing();
    host.run_classic_script("throw new Error('boom')", "throws")
        .expect_err("an uncaught throw is an error");
    host.run_classic_script("console.log('still here')", "after")
        .expect("the isolate is still usable");
    assert_eq!(lines(&sink), vec!["Log: still here".to_owned()]);
}

#[test]
fn a_microtask_runs_before_the_script_call_returns() {
    let (mut host, sink) = capturing();
    host.run_classic_script(
        "Promise.resolve().then(() => console.log('microtask')); console.log('sync')",
        "checkpoint",
    )
    .expect("the script runs");
    assert_eq!(
        lines(&sink),
        vec!["Log: sync".to_owned(), "Log: microtask".to_owned()],
        "the checkpoint is part of the turn, not of the next one",
    );
}

#[test]
fn page_script_has_no_capabilities() {
    let (mut host, _sink) = capturing();
    // Nothing that reaches the host is even *named* in a page isolate: there is
    // no bootstrap that installs a filesystem or a process object. The test
    // that matters is therefore that reaching for one is an error rather than a
    // read, whichever way it fails.
    for reach in [
        "globalThis.Deno?.readTextFileSync('/etc/passwd')",
        "process.env.HOME",
        "require('fs')",
    ] {
        let result = host.run_classic_script(reach, "reaching");
        if let Ok(outcome) = &result {
            assert!(
                outcome.completion == "undefined",
                "a page reached the host: {reach} -> {}",
                outcome.completion,
            );
        }
    }
}

#[test]
fn a_script_that_will_not_stop_is_stopped() {
    let (mut host, _sink) = capturing();
    let started = Instant::now();
    let error = host
        .run_classic_script("while (true) {}", "runaway")
        .expect_err("a runaway script does not finish");
    let elapsed = started.elapsed();

    assert!(
        error.interrupted,
        "it was the watchdog that ended it: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "it was stopped, and near its budget rather than long after: {elapsed:?}",
    );

    // And the isolate is usable afterwards: the interrupt flag was cleared, so
    // the next script is not unwound before it starts.
    host.run_classic_script("1 + 1", "after")
        .expect("the isolate is still usable");
}

/// Parse `html` with a page's scripts attached, and give back what its console
/// said and how many scripts the parser actually ran.
fn parse_running_scripts(html: &str) -> (Vec<String>, usize, usize) {
    let (output, seen, run, _tree) = parse_and_dump(html);
    (output, seen, run)
}

/// The same, plus the tree the scripts left behind.
fn parse_and_dump(html: &str) -> (Vec<String>, usize, usize, String) {
    let sink = Arc::new(Captured::default());
    let handle: ConsoleSinkHandle = Arc::clone(&sink) as ConsoleSinkHandle;
    let mut parser = HtmlParser::new().with_script_runner(Box::new(PageScripts::with_console(
        "https://example.com/",
        handle,
    )));
    let mut document = otlyra_dom::Document::new();
    parser.feed(&mut document, html.into());
    // A test has no network, so a parse stopped on a script it does not hold
    // stays stopped. Step over it, as the parse-once helper does.
    while parser.blocked_on().is_some() {
        parser.skip_script(&mut document);
    }
    let (seen, run) = (parser.scripts_seen(), parser.scripts_run());
    let mut runner = parser.finish(&mut document);
    // What the page put on the clock. A test has no event loop to be woken
    // from, so it lets the page settle the way a screenshot does — bounded, or
    // a `setInterval` would keep it here.
    if let Some(runner) = runner.as_mut() {
        runner.settle(&mut document, Duration::from_millis(250));
    }
    let tree = otlyra_dom::dump::serialize(&document);
    (lines(&sink), seen, run, tree)
}

/// Timers run when the page asked for them, in that order, and an interval
/// keeps coming back until it is stopped.
#[test]
fn timers_run_in_the_order_their_deadlines_fall_due() {
    let (output, ..) = parse_running_scripts(
        "<script>\
           setTimeout(() => console.log('third'), 40);\
           setTimeout(() => console.log('first'), 0);\
           setTimeout(() => console.log('second'), 20);\
           const cancelled = setTimeout(() => console.log('never'), 10);\
           clearTimeout(cancelled);\
           let ticks = 0;\
           const every = setInterval(() => {\
             ticks++;\
             console.log('tick', ticks);\
             if (ticks === 3) clearInterval(every);\
           }, 5);\
         </script>",
    );
    // The interval's ticks interleave with the one-shots by deadline, so what is
    // asserted is that each ran, that the one-shots kept their order, and that
    // the interval stopped when it was told to.
    assert!(output.contains(&"Log: first".to_owned()));
    let order: Vec<&String> = output
        .iter()
        .filter(|line| line.ends_with("first") || line.ends_with("second") || line.ends_with("third"))
        .collect();
    assert_eq!(
        order,
        vec!["Log: first", "Log: second", "Log: third"],
        "one-shots run by deadline: {output:?}"
    );
    assert_eq!(
        output.iter().filter(|line| line.starts_with("Log: tick")).count(),
        3,
        "the interval stopped when it cleared itself: {output:?}"
    );
    assert!(
        !output.iter().any(|line| line.ends_with("never")),
        "a cleared timer does not run: {output:?}"
    );
}

/// A timer that schedules another gets it: the wheel is turned until the page
/// stops asking, not once.
#[test]
fn a_timer_may_schedule_another() {
    let (output, ..) = parse_running_scripts(
        "<script>\
           let left = 3;\
           const again = () => {\
             console.log('step', left);\
             if (--left > 0) setTimeout(again, 1);\
           };\
           setTimeout(again, 1);\
         </script>",
    );
    assert_eq!(
        output,
        vec![
            "Log: step 3".to_owned(),
            "Log: step 2".to_owned(),
            "Log: step 1".to_owned(),
        ],
    );
}

/// A timer callback reaches the document, and what it changes is seen as a
/// change — the page is restyled and laid out again because of it.
#[test]
fn a_timer_may_change_the_document() {
    let (_output, _seen, _run, tree) = parse_and_dump(
        "<body><p id=x>before</p>\
         <script>setTimeout(() => { document.getElementById('x').textContent = 'after'; }, 1);</script>",
    );
    assert!(tree.contains("\"after\""), "the timer rewrote the node:\n{tree}");
}

/// A node has one wrapper, and the page is handed that one every time.
///
/// Without it `el === el` is false, a `Set` of elements fills with duplicates,
/// and every listener a page hangs on an element is hung on an object nobody
/// will be given again.
#[test]
fn one_node_has_one_wrapper() {
    let (output, ..) = parse_running_scripts(
        "<body><div id=x><span></span></div>\
         <script>\
           const a = document.getElementById('x');\
           const b = document.querySelector('#x');\
           console.log('same', a === b);\
           console.log('body', document.body === document.body);\
           console.log('document', document === document);\
           console.log('parent', a.firstElementChild.parentElement === a);\
           console.log('set', new Set([a, b, document.querySelector('div')]).size);\
           a.marked = 7;\
           console.log('kept', document.querySelector('#x').marked);\
         </script>",
    );
    assert_eq!(
        output,
        vec![
            "Log: same true".to_owned(),
            "Log: body true".to_owned(),
            "Log: document true".to_owned(),
            "Log: parent true".to_owned(),
            "Log: set 1".to_owned(),
            "Log: kept 7".to_owned(),
        ],
    );
}

/// An event goes down to its target and back up, and a listener on a container
/// hears about what happened inside it. Every list, menu and grid on the web is
/// written this way.
#[test]
fn an_event_captures_down_to_its_target_and_bubbles_back_up() {
    let (output, ..) = parse_running_scripts(
        "<body><div id=outer><div id=inner><button id=go></button></div></div>\
         <script>\
           const outer = document.getElementById('outer');\
           const inner = document.getElementById('inner');\
           const go = document.getElementById('go');\
           const seen = [];\
           const note = (name) => (event) => seen.push(name + ':' + event.eventPhase);\
           window.addEventListener('click', note('window-capture'), true);\
           outer.addEventListener('click', note('outer-capture'), true);\
           inner.addEventListener('click', note('inner-capture'), { capture: true });\
           go.addEventListener('click', note('target-capture'), true);\
           go.addEventListener('click', note('target'));\
           inner.addEventListener('click', note('inner-bubble'));\
           outer.addEventListener('click', note('outer-bubble'));\
           window.addEventListener('click', note('window-bubble'));\
           go.dispatchEvent(new Event('click', { bubbles: true }));\
           console.log(seen.join(' '));\
         </script>",
    );
    assert_eq!(
        output,
        vec![
            [
                "Log: window-capture:1 outer-capture:1 inner-capture:1",
                "target-capture:2 target:2",
                "inner-bubble:3 outer-bubble:3 window-bubble:3",
            ]
            .join(" ")
        ],
    );
}

/// An event that does not bubble is heard where it landed and nowhere else,
/// and one that is stopped goes no further.
#[test]
fn a_non_bubbling_event_stops_at_its_target_and_stopping_ends_the_walk() {
    let (output, ..) = parse_running_scripts(
        "<body><div id=box><span id=leaf></span></div>\
         <script>\
           const box = document.getElementById('box');\
           const leaf = document.getElementById('leaf');\
           let heard = [];\
           box.addEventListener('focus', () => heard.push('box'));\
           leaf.addEventListener('focus', () => heard.push('leaf'));\
           leaf.dispatchEvent(new Event('focus'));\
           console.log('quiet', heard.join(','));\
\
           heard = [];\
           box.addEventListener('ping', () => heard.push('box'));\
           leaf.addEventListener('ping', (event) => { heard.push('leaf'); event.stopPropagation(); });\
           leaf.dispatchEvent(new Event('ping', { bubbles: true }));\
           console.log('stopped', heard.join(','));\
\
           heard = [];\
           leaf.addEventListener('pong', (event) => { heard.push('one'); event.stopImmediatePropagation(); });\
           leaf.addEventListener('pong', () => heard.push('two'));\
           box.addEventListener('pong', () => heard.push('box'));\
           leaf.dispatchEvent(new Event('pong', { bubbles: true }));\
           console.log('immediate', heard.join(','));\
         </script>",
    );
    assert_eq!(
        output,
        vec![
            "Log: quiet leaf".to_owned(),
            "Log: stopped leaf".to_owned(),
            "Log: immediate one".to_owned(),
        ],
    );
}

/// What a listener is handed while it runs, and what the event says afterwards.
#[test]
fn an_event_reports_its_target_its_path_and_whether_it_was_cancelled() {
    let (output, ..) = parse_running_scripts(
        "<body><div id=box><span id=leaf></span></div>\
         <script>\
           const box = document.getElementById('box');\
           const leaf = document.getElementById('leaf');\
           box.addEventListener('tap', (event) => {\
             console.log('target', event.target === leaf);\
             console.log('current', event.currentTarget === box);\
             console.log('path', event.composedPath()[0] === leaf,\
                          event.composedPath().includes(window));\
             event.preventDefault();\
           });\
           const went = leaf.dispatchEvent(new Event('tap', { bubbles: true, cancelable: true }));\
           console.log('cancelled', went === false);\
           const plain = new Event('tap', { bubbles: true });\
           console.log('uncancelable', leaf.dispatchEvent(plain) === true);\
           console.log('after', plain.currentTarget === null, plain.eventPhase === 0);\
         </script>",
    );
    assert_eq!(
        output,
        vec![
            "Log: target true".to_owned(),
            "Log: current true".to_owned(),
            "Log: path true true".to_owned(),
            "Log: cancelled true".to_owned(),
            "Log: target true".to_owned(),
            "Log: current true".to_owned(),
            "Log: path true true".to_owned(),
            "Log: uncancelable true".to_owned(),
            "Log: after true true".to_owned(),
        ],
    );
}

/// A `once` listener runs once whichever phase it is in, and removing one that
/// has not run yet stops it running.
#[test]
fn once_removes_a_listener_and_a_removal_mid_dispatch_is_honoured() {
    let (output, ..) = parse_running_scripts(
        "<body><div id=box><span id=leaf></span></div>\
         <script>\
           const box = document.getElementById('box');\
           const leaf = document.getElementById('leaf');\
           let count = 0;\
           box.addEventListener('tick', () => count++, { once: true });\
           leaf.dispatchEvent(new Event('tick', { bubbles: true }));\
           leaf.dispatchEvent(new Event('tick', { bubbles: true }));\
           console.log('once', count);\
\
           const later = () => console.log('should not run');\
           box.addEventListener('tock', () => box.removeEventListener('tock', later));\
           box.addEventListener('tock', later);\
           box.dispatchEvent(new Event('tock'));\
           console.log('removed');\
         </script>",
    );
    assert_eq!(
        output,
        vec!["Log: once 1".to_owned(), "Log: removed".to_owned()],
    );
}

/// The table holds one entry per node and not one per lookup, past the point
/// where it sweeps itself.
#[test]
fn the_wrapper_table_holds_one_entry_per_node() {
    let mut html = String::from("<body>");
    for index in 0..600 {
        html.push_str(&format!("<p id=p{index}></p>"));
    }
    // Every element looked up twice, so a table that kept a wrapper per lookup
    // would hold twice what it should.
    html.push_str(
        "<script>\
           for (let i = 0; i < 600; i++) {\
             document.getElementById('p' + i);\
             document.getElementById('p' + i);\
           }\
           console.log('done');\
         </script>",
    );
    let (output, ..) = parse_running_scripts(&html);
    assert_eq!(output, vec!["Log: done".to_owned()]);
    let held = otlyra_script::dom::wrapper_count();
    assert!(
        held <= 610,
        "one wrapper per node and a few for the document: {held}"
    );
}

/// What identity is actually for: a listener is hung on the wrapper, so a page
/// that registers one through a lookup and fires it through another must be
/// talking about the same object both times.
#[test]
fn a_listener_registered_through_one_lookup_fires_through_another() {
    let (output, ..) = parse_running_scripts(
        "<body><button id=go></button>\
         <script>\
           document.getElementById('go')\
             .addEventListener('click', () => console.log('heard it'));\
           document.querySelector('#go').dispatchEvent(new Event('click'));\
         </script>",
    );
    assert_eq!(output, vec!["Log: heard it".to_owned()]);
}

/// And two nodes do not share one, which is the other half of the property.
#[test]
fn two_nodes_have_two_wrappers() {
    let (output, ..) = parse_running_scripts(
        "<body><p id=a></p><p id=b></p>\
         <script>\
           const a = document.getElementById('a');\
           const b = document.getElementById('b');\
           console.log('distinct', a !== b);\
           console.log('created', document.createElement('p') !== document.createElement('p'));\
         </script>",
    );
    assert_eq!(
        output,
        vec![
            "Log: distinct true".to_owned(),
            "Log: created true".to_owned(),
        ],
    );
}

#[test]
fn what_a_page_defers_runs_when_the_document_is_finished() {
    let (output, ..) = parse_running_scripts(
        "<script>\
           document.addEventListener('DOMContentLoaded', () => console.log('ready'));\
           window.addEventListener('load', () => console.log('loaded'));\
           setTimeout(() => console.log('later'), 0);\
           const cancelled = setTimeout(() => console.log('never'), 0);\
           clearTimeout(cancelled);\
           requestAnimationFrame(() => console.log('frame'));\
           console.log('parsing', document.readyState);\
         </script>",
    );
    assert_eq!(
        output,
        vec![
            "Log: parsing loading".to_owned(),
            "Log: ready".to_owned(),
            "Log: loaded".to_owned(),
            "Log: frame".to_owned(),
            "Log: later".to_owned(),
        ],
    );
}

#[test]
fn an_external_script_whose_source_is_in_hand_runs_where_the_document_names_it() {
    let sink = Arc::new(Captured::default());
    let handle: ConsoleSinkHandle = Arc::clone(&sink) as ConsoleSinkHandle;
    let mut sources = otlyra_html::ExternalSources::new();
    sources.insert(
        "app.js".to_owned(),
        "window.greet = () => console.log('from the bundle')".to_owned(),
    );
    let (parsed, _runner) = otlyra_html::parse_with_scripts(
        b"<body><script src=\"app.js\"></script>\
          <script>greet()</script>",
        Some("utf-8"),
        Some(Box::new(PageScripts::with_console(
            "https://example.com/",
            handle,
        ))),
        sources,
    );

    assert_eq!(parsed.external_scripts.len(), 1);
    assert!(
        parsed.deferred_scripts.is_empty(),
        "nothing was left for the caller to run late",
    );
    assert_eq!(
        lines(&sink),
        vec!["Log: from the bundle".to_owned()],
        "the inline script after it could call what it defined",
    );
}

#[test]
fn an_external_script_runs_after_the_parse_and_the_load_events_wait_for_it() {
    let sink = Arc::new(Captured::default());
    let handle: ConsoleSinkHandle = Arc::clone(&sink) as ConsoleSinkHandle;
    let (mut parsed, runner) = otlyra_html::parse_with_scripts(
        b"<body><script>document.addEventListener('DOMContentLoaded', () => console.log('inline heard it'))</script>\
          <script src=\"app.js\"></script>",
        Some("utf-8"),
        Some(Box::new(PageScripts::with_console(
            "https://example.com/",
            handle,
        ))),
        otlyra_html::ExternalSources::new(),
    );
    let mut runner = runner.expect("the runner comes back out");
    assert_eq!(
        parsed.deferred_scripts.len(),
        1,
        "the parse names the script it did not run",
    );
    assert_eq!(
        lines(&sink),
        Vec::<String>::new(),
        "nothing fired while a script was still to come",
    );

    let node = parsed.deferred_scripts[0];
    runner.run_external(
        "document.addEventListener('DOMContentLoaded', () => console.log('external heard it too'))",
        node,
        &mut parsed.document,
    );
    runner.document_finished(&mut parsed.document, false);

    assert_eq!(
        lines(&sink),
        vec![
            "Log: inline heard it".to_owned(),
            "Log: external heard it too".to_owned(),
        ],
        "the events waited for the external script, and both listeners heard them",
    );
}

#[test]
fn an_element_has_a_style_a_page_can_read_and_write() {
    let (output, _seen, _run, tree) = parse_and_dump(
        "<body><p id=one>text</p><script>\
           const p = document.getElementById('one');\
           p.style.color = 'red';\
           p.style.setProperty('margin-top', '4px');\
           console.log(p.style.color, p.style.marginTop, 'animation' in p.style);\
         </script>",
    );
    assert_eq!(output, vec!["Log: red 4px true".to_owned()]);
    assert!(
        tree.contains("style=\"color: red; margin-top: 4px\""),
        "the declarations are written back to the attribute: {tree}",
    );
}

#[test]
fn window_is_the_global_object() {
    let (output, ..) = parse_running_scripts(
        "<script>console.log(window === globalThis, self === window, top === window)</script>",
    );
    assert_eq!(output, vec!["Log: true true true".to_owned()]);
}

#[test]
fn script_reads_the_document_it_is_in() {
    let (output, ..) = parse_running_scripts(
        "<title>a page</title><p id=one class='x y'>text</p>\
         <script>\
           console.log(document.title);\
           const p = document.getElementById('one');\
           console.log(p.tagName, p.className, p.textContent);\
           console.log(p.classList.contains('y'), document.querySelectorAll('p').length);\
         </script>",
    );
    assert_eq!(
        output,
        vec![
            "Log: a page".to_owned(),
            "Log: P x y text".to_owned(),
            "Log: true 1".to_owned(),
        ],
    );
}

#[test]
fn script_changes_the_document_it_is_in() {
    let (_output, _seen, _run, tree) = parse_and_dump(
        "<body><p id=one>old</p>\
         <script>\
           document.getElementById('one').textContent = 'new';\
           document.getElementById('one').setAttribute('data-done', 'yes');\
           const added = document.createElement('span');\
           added.textContent = 'added';\
           document.body.appendChild(added);\
           document.documentElement.classList.add('js');\
         </script>",
    );
    assert!(tree.contains("\"new\""), "textContent replaced: {tree}");
    assert!(tree.contains("data-done=\"yes\""), "attribute set: {tree}");
    assert!(tree.contains("<span>"), "element appended: {tree}");
    assert!(tree.contains("class=\"js\""), "class added: {tree}");
    assert!(!tree.contains("\"old\""), "the old text is gone: {tree}");
}

#[test]
fn a_listener_registered_on_the_document_can_be_dispatched_to() {
    let (output, ..) = parse_running_scripts(
        "<script>\
           document.addEventListener('ping', (e) => console.log('heard', e.type));\
           document.dispatchEvent(new Event('ping'));\
         </script>",
    );
    assert_eq!(output, vec!["Log: heard ping".to_owned()]);
}

#[test]
fn inline_scripts_in_a_document_run_in_document_order() {
    let (output, seen, run) = parse_running_scripts(
        "<!doctype html><title>t</title>\
         <script>var x = 1; console.log('first')</script>\
         <script>console.log('second', x + 1)</script>",
    );
    assert_eq!((seen, run), (2, 2));
    assert_eq!(
        output,
        vec!["Log: first".to_owned(), "Log: second 2".to_owned()],
    );
}

#[test]
fn a_failing_script_does_not_stop_the_ones_after_it() {
    let (output, seen, run) = parse_running_scripts(
        "<script>function (</script><script>throw new Error('boom')</script>\
         <script>console.log('third')</script>",
    );
    assert_eq!((seen, run), (3, 3));
    assert_eq!(output, vec!["Log: third".to_owned()]);
}

#[test]
fn an_external_or_non_javascript_script_is_not_run() {
    let (output, seen, run) = parse_running_scripts(
        r#"<script src="app.js"></script>
           <script type="module">import "./m.js"</script>
           <script type="text/template"><div></div></script>
           <script type="text/javascript;charset=utf-8">console.log('classic')</script>"#,
    );
    assert_eq!(
        (seen, run),
        (4, 1),
        "the parser stops at all four and runs only the classic inline one",
    );
    assert_eq!(output, vec!["Log: classic".to_owned()]);
}
