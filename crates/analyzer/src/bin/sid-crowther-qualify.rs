#[path = "support/native_qualify.rs"]
mod native_qualify;

fn main() {
    native_qualify::main(native_qualify::FamilySpec {
        driver: "Antony_Crowther_V3",
        label: "Crowther",
        default_frames: 400,
    });
}
