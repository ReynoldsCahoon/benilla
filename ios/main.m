// The app's C entry: hands over to benilla (`main_rs`, crates/benilla-ios), whose winit event loop
// runs UIApplicationMain and never returns.
extern void main_rs(void);

int main(void) {
    main_rs();
    return 0;
}
