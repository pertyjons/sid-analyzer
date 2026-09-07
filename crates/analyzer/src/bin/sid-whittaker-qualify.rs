#[path = "support/native_qualify.rs"]
mod native_qualify;

fn main() {
    native_qualify::main(native_qualify::FamilySpec {
        driver: "David_Whittaker",
        label: "Whittaker",
        default_frames: 400,
    });
}
