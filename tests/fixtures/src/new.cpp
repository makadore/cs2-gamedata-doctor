// The "new build" fixture. Differences from old.cpp:
//   Base gains alpha_prime between alpha and beta  -> beta/gamma/delta shift +1 (also in Derived, Both, Combo)
//   Base::helper's body changes (NOT virtual)       -> must not count as a change of any slot
//   Derived::zeta's body changes                    -> changed in place, after the shift
//   Thing gains five at the end                     -> appended slot, no shift
//   Everything else is identical.
#include <cstdint>

#ifdef _MSC_VER
#define KEEP __declspec(noinline)
#define EXPORT __declspec(dllexport)
#else
#define KEEP __attribute__((noinline))
#define EXPORT __attribute__((visibility("default")))
#endif

struct Base {
    virtual ~Base() {}
    virtual int alpha(int x);
    virtual int alpha_prime(int x);
    virtual int beta(int x);
    virtual int gamma(int x);
    virtual int delta(int x);
    int helper(int x);
    int field = 7;
};

struct Derived : Base {
    int alpha(int x) override;
    virtual int epsilon(int x);
    virtual int zeta(int x);
};

struct IThing {
    virtual ~IThing();
    virtual int one(int x) = 0;
    virtual int two(int x) = 0;
    virtual int three(int x) = 0;
};

struct Thing : IThing {
    int one(int x) override;
    int two(int x) override;
    int three(int x) override;
    virtual int four(int x);
    virtual int five(int x);
};

struct Mixin {
    virtual ~Mixin() {}
    virtual int mix(int x);
};

struct Both : Base, Mixin {
    int beta(int x) override;
    int mix(int x) override;
};

struct IExtra {
    virtual ~IExtra();
    virtual int extra(int x) = 0;
};

struct Combo : Base, IExtra {
    int extra(int x) override;
};

// Same shape as in old.cpp, but left and right swapped their call targets.
int target_a(int x);
int target_b(int x);
struct Twins {
    virtual ~Twins();
    virtual int left(int x);
    virtual int right(int x);
};

// One method of unique shape whose call target changes between builds: the known blind spot
// of fingerprint matching (masked call targets), documented by a test.
struct Solo {
    virtual ~Solo();
    virtual int only(int x);
};

// A base reached only through virtual inheritance: its subobject offset is not static, so an
// entry on it must come back as "not placed", never as a guessed index.
struct VBase {
    virtual ~VBase();
    virtual int v(int x) = 0;
};

struct Diamond : virtual VBase {
    int v(int x) override;
};

// A second, honest implementation of IThing (consensus must count both) and an implementation
// of IExtra through a virtual base (which must block IExtra's verdict).
struct Thing2 : IThing {
    int one(int x) override;
    int two(int x) override;
    int three(int x) override;
};

struct VExtra : virtual IExtra {
    int extra(int x) override;
};


// --- definitions, in the order they are laid out in .text ---------------------------------
KEEP int Base::alpha(int x) { return x * 3 + 1; }
KEEP int Base::helper(int x) { int s = x; for (int i = 0; i < 3; i++) s = s * 7 + i; return s + 2000; }
KEEP int Base::alpha_prime(int x) { return x * 7 + 3; }
KEEP int Base::beta(int x) { return (x ^ 0x55) - 7; }
KEEP int Base::gamma(int x) { int s = 0; for (int i = 0; i < x; i++) s += i * i; return s; }
KEEP int Base::delta(int x) { return x < 0 ? -x : x + field; }

KEEP int Derived::alpha(int x) { return x * 5 - 2; }
KEEP int Derived::epsilon(int x) { return (x << 2) | 1; }
KEEP int Derived::zeta(int x) { return x * 11 + 13 + (x >> 3); }

IThing::~IThing() {}
KEEP int Thing::one(int x) { return x + 100; }
KEEP int Thing::two(int x) { return x * x + 200; }
KEEP int Thing::three(int x) { return (x % 7) + 300; }
KEEP int Thing::four(int x) { return 400 - x; }
KEEP int Thing::five(int x) { return 500 + x * 2; }

KEEP int Mixin::mix(int x) { return x * 17 % 1000; }
KEEP int Both::beta(int x) { return x * 19 + 23; }
KEEP int Both::mix(int x) { return (x + 29) * 31; }

IExtra::~IExtra() {}
KEEP int Combo::extra(int x) { return x * 37 - 41; }

KEEP int target_a(int x) { return x + 1; }
KEEP int target_b(int x) { return x * 2 + 5; }
Twins::~Twins() {}
KEEP int Twins::left(int x) { return target_b(x); }
KEEP int Twins::right(int x) { return target_a(x); }

Solo::~Solo() {}
KEEP int Solo::only(int x) { return target_b(x) ^ 0x3c; }

VBase::~VBase() {}
KEEP int Diamond::v(int x) { return x * 43 + 1; }
KEEP int Thing2::one(int x) { return x - 100; }
KEEP int Thing2::two(int x) { return x * x - 200; }
KEEP int Thing2::three(int x) { return (x % 11) - 300; }
KEEP int VExtra::extra(int x) { return x * 47 + 3; }

// unchanged from old.cpp: the abstract class with one implemented method
struct IHalf {
    virtual ~IHalf() {}
    virtual int real(int x);
    virtual int pure(int x) = 0;
};
KEEP int IHalf::real(int x) { return x * 7 + 3; }
struct Half : IHalf {
    int pure(int x) override { return x - 11; }
};

extern "C" {
EXPORT IHalf* make_half() { return new Half; }
EXPORT Twins* make_twins() { return new Twins; }
EXPORT VBase* make_diamond() { return new Diamond; }
EXPORT IThing* make_thing2() { return new Thing2; }
EXPORT IExtra* make_vextra() { return new VExtra; }
EXPORT Solo* make_solo() { return new Solo; }
EXPORT Base* make_base() { return new Base; }
EXPORT Derived* make_derived() { return new Derived; }
EXPORT IThing* make_thing() { return new Thing; }
EXPORT Mixin* make_mixin() { return new Mixin; }
EXPORT Both* make_both() { return new Both; }
EXPORT IExtra* make_combo() { return new Combo; }
EXPORT int use_helper(Base* b, int x) { return b->helper(x); }
}
