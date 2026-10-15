use super::*;

struct Announce {
    inner: Box<dyn otlyra_html::ScriptRunner>,
    started: Option<mpsc::Sender<()>>,
}
impl otlyra_html::ScriptRunner for Announce {
    fn run(&mut self, source: &str, node: otlyra_dom::NodeId, document: &mut otlyra_dom::Document) {
        self.inner.run(source, node, document);
    }
    fn next_deadline(&self) -> Option<Instant> {
        self.inner.next_deadline()
    }
    fn take_mutated(&mut self) -> bool {
        self.inner.take_mutated()
    }
    fn run_due_timers(&mut self, document: &mut otlyra_dom::Document) -> usize {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        self.inner.run_due_timers(document)
    }
}

fn queue_sample() -> (f64, usize) {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = WindowBrowser::spawn(viewport, false, |text| {
        Browser::with_fetcher_and_text(Fetcher::spawn(Fields), Default::default(), text)
    })
    .unwrap();
    ready(&mut window);
    let (started, running) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            let (parsed, runner) = otlyra_html::parse_with_scripts(
                br#"<p id=state done=0></p><script>
          const state = document.getElementById('state');
          let completed = 0;
          for (let i = 0; i < 200; i++) setTimeout(() => {
            const until = Date.now() + 1;
            while (Date.now() < until) {}
            state.setAttribute('done', String(++completed));
          }, 0);
          state.setAttribute('armed', '200');
        </script>"#,
                Some("utf-8"),
                Some(Box::new(otlyra_script::PageScripts::new(
                    "https://timer-queue.test/",
                ))),
                Default::default(),
            );
            let state = otlyra_css::stylo_dom::select(&parsed.document, "#state").unwrap()[0];
            assert_eq!(parsed.document.attr(state, "armed"), Some("200"));
            let tab = browser.active_id();
            browser
                .world
                .set_page(tab, Some(crate::page::PageScene::new(parsed.document)));
            browser.world.set_scripts(
                tab,
                Box::new(Announce {
                    inner: runner.unwrap(),
                    started: Some(started),
                }),
            );
        })))
        .unwrap();
    running.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    let (sample, received) = mpsc::channel();
    window
        .sender
        .send(Command::Task(Box::new(move |browser| {
            let document = browser.active_page().unwrap().document();
            let state = otlyra_css::stylo_dom::select(document, "#state").unwrap()[0];
            let done: usize = document.attr(state, "done").unwrap().parse().unwrap();
            sample.send(done).unwrap();
        })))
        .unwrap();
    let done = received.recv_timeout(Duration::from_secs(5)).unwrap();
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    window.shutdown().unwrap();
    (elapsed, done)
}

#[test]
fn queued_content_commands_interrupt_a_large_due_timer_cohort() {
    let (_, done) = queue_sample();
    assert!(
        done < 200,
        "content command waited for the complete timer cohort"
    );
}

#[test]
#[ignore = "controlled content-queue measurement"]
fn measure_timer_queue_ack() {
    let (elapsed_ms, done) = queue_sample();
    eprintln!(
        "OTLYRA_TIMER_QUEUE_SAMPLE {}",
        serde_json::json!({"elapsed_ms":elapsed_ms,"callbacks_before_ack":done,"cohort":200})
    );
}
