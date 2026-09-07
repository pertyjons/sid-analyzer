#[path = "support/native_qualify.rs"]
mod native_qualify;

fn main() {
    native_qualify::main(native_qualify::FamilySpec {
        driver: "Rob_Hubbard",
        label: "Hubbard",
        default_frames: 400,
    });
}
