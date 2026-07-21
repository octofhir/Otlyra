use otlyra_text::{FontStack, GenericFamily, TextEngine, TextSpan};

fn main() {
    let mut engine = TextEngine::new();
    for size in [12.0f32, 16.0, 17.0, 19.0, 20.0, 21.0, 24.0, 32.0] {
        let stack = FontStack::generic(GenericFamily::SystemUi);
        let span = TextSpan {
            text: "The quick brown fox jumps over the lazy dog 0123456789",
            font_stack: stack,
            font_size: size,
            font_weight: 400,
            italic: false,
            underline: false,
            strikethrough: false,
            brush: [0, 0, 0, 255],
            line_height: None,
        };
        let shaped = engine.shape_spans(&[span], &[], None);
        let run = &shaped.runs[0];
        println!(
            "size={size} total={:.4} per-em={:.5} coords={:?} index={} bytes={}",
            shaped.metrics.width,
            shaped.metrics.width / size,
            run.normalized_coords,
            run.font.index,
            run.font.data.as_ref().len(),
        );
    }
}
