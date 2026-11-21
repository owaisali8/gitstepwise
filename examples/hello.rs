use gpui_kit::component::button::*;
use gpui_kit::component::*;
use gpui_kit::*;

struct HelloWorld {
    count: usize,
}

impl Render for HelloWorld {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_3()
            .size_full()
            .items_center()
            .justify_center()
            .child(format!("Clicked {} times", self.count))
            .child(
                Button::new("inc")
                    .primary()
                    .label("Click me")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.count += 1;
                        cx.notify();
                    })),
            )
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| HelloWorld { count: 0 })
            })
            .expect("failed to open window");
        });
}
