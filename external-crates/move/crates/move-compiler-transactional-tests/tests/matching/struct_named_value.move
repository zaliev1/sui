//# init --edition development

//# publish
module 0x42::m {
    public struct A { x: u64 }

    fun t00(s: A): u64 {
        match (s) {
            A { x: 0 } => 0,
            A { x } => x,
        }
    }

    public fun run() {
        let a = A { x: 42 };
        assert!(a.t00() == 42);

        let b = A { x: 0 };
        assert!(b.t00() == 0);
    }
}

//# run
module 0x42::main {
    use 0x42::m;
    fun main() {
        m::run()
    }
}
