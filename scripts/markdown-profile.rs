// Included by profile-markdown.sh after selecting baseline/current parser modules.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Meter;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: Meter = Meter;

unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(live, Relaxed);
            TOTAL.fetch_add(layout.size(), Relaxed);
            COUNT.fetch_add(1, Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let source = std::fs::read_to_string(&args[1]).unwrap();
    let baseline_live = LIVE.load(Relaxed);
    TOTAL.store(0, Relaxed);
    COUNT.store(0, Relaxed);
    PEAK.store(baseline_live, Relaxed);
    let start = Instant::now();
    let mut parser = markdown::parser::IncrementalParser::new();
    let mut previous_frame = None;
    let mut end = 0;
    let mut commits = 0;
    while end < source.len() {
        end = (end + 160).min(source.len());
        while !source.is_char_boundary(end) { end += 1; }
        parser.set_text(&source[..end]);
        let next = parser.display_tree();
        // A renderer still owns the old frame while the new tree is derived.
        std::hint::black_box(&previous_frame);
        previous_frame = Some(next);
        commits += 1;
    }
    let elapsed = start.elapsed();
    let retained = LIVE.load(Relaxed) - baseline_live;
    let total = TOTAL.load(Relaxed);
    let peak = PEAK.load(Relaxed) - baseline_live;
    let count = COUNT.load(Relaxed);
    assert_eq!(parser.tree(), &markdown::parser::parse_full(&source));
    println!("{{\"source_bytes\":{},\"commits\":{},\"elapsed_ms\":{},\"allocated_bytes\":{},\"allocations\":{},\"peak_live_bytes\":{},\"retained_bytes\":{}}}",
        source.len(), commits, elapsed.as_secs_f64() * 1000.0, total, count, peak, retained);
}
