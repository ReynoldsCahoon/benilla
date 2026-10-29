//! Stamps the commit this library was built from, as the desktop launcher's build script does.

fn main() {
    benilla_buildstamp::emit();
}
