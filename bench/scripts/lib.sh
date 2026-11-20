# Shared helpers of the bench scripts (sourced, not run). Bash 3.2 compatible.
# Needs BENCH_SRC (the bench/ folder); go_selected also reads SEL_IDS and FILTER.

has_word() { case " $1 " in *" $2 "*) return 0 ;; esac; return 1; }

# Path for native Windows programs (cargo, go, the bench exe): /d/x -> D:/x.
native() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; return; fi ;;
    esac
    printf '%s\n' "$1"
}

# module -> benchmark IDs (bench-plan.md section 7); Phase 6 batches rerun by module.
BENCH_MODULES="convert codec proto fec transport secret audio capture clipboard agent viewer input files relay signal store server"
module_ids() {
    case "$1" in
        convert)   echo "B-MICRO-01 B-MICRO-02 B-MICRO-03 B-MICRO-07" ;;
        codec)     echo "B-MICRO-04 B-MICRO-05 B-MICRO-06 B-MICRO-07 B-MICRO-08 B-MICRO-09 B-MICRO-10 B-MACRO-01 B-MACRO-02 B-MACRO-03" ;;
        proto)     echo "B-MICRO-11" ;;
        fec)       echo "B-MICRO-12 B-MICRO-13 B-MACRO-02" ;;
        transport) echo "B-MICRO-12 B-MICRO-13 B-MICRO-16 B-MACRO-01 B-MACRO-02 B-MACRO-04 B-MACRO-05 B-MACRO-06 B-MACRO-07 B-MACRO-08" ;;
        secret)    echo "B-MICRO-14" ;;
        audio)     echo "B-MICRO-15" ;;
        capture)   echo "B-MICRO-17 B-MACRO-03 B-MACRO-08" ;;
        clipboard) echo "B-MICRO-18" ;;
        agent)     echo "B-MACRO-03 B-MACRO-04 B-MACRO-05 B-MACRO-06 B-MACRO-07 B-MACRO-08 B-MACRO-09 B-MACRO-10 B-RES-01 B-RES-02 B-RES-03 B-RES-04" ;;
        viewer)    echo "B-MICRO-10 B-MACRO-09" ;;
        input)     echo "B-MACRO-08" ;;
        files)     echo "B-MACRO-10 B-RES-03" ;;
        relay)     echo "B-GO-01 B-GO-02 B-GO-03 B-GO-04 B-GO-05 B-RES-05 B-MACRO-06" ;;
        signal)    echo "B-GO-06 B-GO-07 B-MACRO-05 B-MACRO-07" ;;
        store)     echo "B-GO-08" ;;
        server)    echo "B-GO-01 B-GO-02 B-GO-03 B-GO-04 B-GO-05 B-GO-06 B-GO-07 B-GO-08 B-RES-05 B-MACRO-05 B-MACRO-06 B-MACRO-07" ;;
        *) return 1 ;;
    esac
}

# Suite of a benchmark ID.
suite_of() {
    case "$1" in
        B-MICRO-*) echo micro ;; B-MACRO-*) echo macro ;; B-RES-05) echo go-soak ;; B-RES-01) echo long ;; B-RES-*) echo res ;; B-GO-*) echo go ;;
        *) return 1 ;;
    esac
}

# Prints "package Function" for every Go benchmark (bench/go/<package>/bench_*_test.go)
# whose ID is in SEL_IDS (all when empty) and whose "ID/Function" matches FILTER.
go_selected() {
    local f pkg id name
    for f in "$BENCH_SRC"/go/*/bench_*_test.go; do
        [ -f "$f" ] || continue
        pkg="$(basename "$(dirname "$f")")"
        awk -v pkg="$pkg" '
            /^\/\/ BENCH-ID:/ { id = $3; next }
            /^func Benchmark/ { name = $2; sub(/^Benchmark/, "", name); sub(/\(.*/, "", name);
                                print pkg, id, name; id = ""; next }
            /^[^\/]/ { id = "" }
        ' "$f"
    done | while read -r pkg id name; do
        if [ -n "${SEL_IDS:-}" ] && ! has_word "$SEL_IDS" "$id"; then continue; fi
        if [ -n "${FILTER:-}" ] && ! echo "$id/$name" | grep -Eq -- "$FILTER"; then continue; fi
        echo "$pkg $name"
    done
}

# "^Benchmark(A|B)$" for one package, empty when nothing is selected there.
go_bench_regex() {
    local names
    names="$(go_selected | awk -v p="$1" '$1 == p { print $2 }' | tr '\n' '|')"
    names="${names%|}"
    [ -n "$names" ] && echo "^Benchmark($names)\$"
}
