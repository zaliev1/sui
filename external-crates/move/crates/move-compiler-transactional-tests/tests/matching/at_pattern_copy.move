//# init --edition development

//# publish
module 0x42::m {

    public enum Maybe<T> has copy, drop {
        Just(T),
        Nothing
    }
    
    public fun run() {
        let x = Maybe::Just(42);
        let y = Maybe::Nothing;
        let z = test_00(x);
        let w = test_00(y);
        assert!(z == Maybe::Just(42));
        assert!(w == Maybe::Nothing);
    }

    public fun test_00(x: Maybe<u64>): Maybe<u64> {
        match (x) {
            just @ Maybe::Just(x) => if (x > 0) { just } else { Maybe::Just(x * 2) },
            x @ Maybe::Nothing => x
        }
    }

}

//# run
module 0x42::main {
    use 0x42::m;
    fun main() {
        m::run();
    }
}
