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

    fun t01(s: &A, default: &u64): &u64 {
        match (s) {
            A { x: 0 } => default,
            A { x } => x,
        }
    }

    fun t02(s: &mut A, default: &mut u64): &mut u64 {
        match (s) {
            A { x: 0 } => default,
            A { x } => x,
        }
    }

    public fun run() {
        let mut a = A { x: 42 };
        let mut b = A { x: 0 };
        let mut c = A { x: 1 };

        let d = &a;
        let e = &b;
        let f = &c;

        assert!(*d.t01(&0) == 42);
        assert!(*e.t01(&0) == 0);
        assert!(*f.t01(&0) == 1);

        assert!(*a.t02(&mut 0) == 42);
        assert!(*b.t02(&mut 0) == 0);
        assert!(*c.t02(&mut 0) == 1);

        assert!(a.t00() == 42);
        assert!(b.t00() == 0);
        assert!(c.t00() == 1);

        // let A { x: _ } = a;
        // let A { x: _ } = b;
        // let A { x: _ } = c;
    }
}


//# run
module 0x42::main {
    use 0x42::m;
    fun main() {
        m::run()
    }
}
