//# init --edition development

//# publish
module 0x42::m {

    public enum ABC<T> has drop {
        A(T),
        B(T, T),
        C(T, T, T),
    }

    public fun t0(): u64 {
        let subject = ABC::A(0);
        match (subject) {
            ABC::C(x, _, _) | ABC::B(_, x) | ABC::A(x) if (x == &0) => x,
            ABC::A(x) | ABC::C(x, _, _) | ABC::B(_, x)  if (x == &1) => x,
            _ => 1,
        }
    }
}

//# run
module 0x42::main {
    use 0x42::m;
    fun main() {
        assert!(m::t0() == 0);
    }
}
