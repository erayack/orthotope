use orthotope::OrthotopeGlobalAlloc;

#[global_allocator]
static GLOBAL: OrthotopeGlobalAlloc = OrthotopeGlobalAlloc::new();

fn main() {
    let mut values = Vec::new();
    for i in 0..256_u32 {
        values.push(i);
    }

    let text = String::from("orthotope");
    let boxed = Box::new(42_u64);

    assert_eq!(values.len(), 256);
    assert_eq!(values[255], 255);
    assert_eq!(text, "orthotope");
    assert_eq!(*boxed, 42);
}
