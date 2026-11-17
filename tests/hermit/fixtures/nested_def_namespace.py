# Fixture: Method count must use node.body, not ast.walk (#13884)
# AlgorithmRunner has 2 real methods; the nested helper defs inside run()
# made the old ast.walk-based count see 11 defs and suppress the flag via
# the 10+-method namespace exclusion. Expected: FLAGGED.
class AlgorithmRunner:
    def run(self, seed):
        def derive(value):
            return value ^ seed

        def mix(value, round_no):
            return derive(value) + round_no

        def trim(value):
            return value & 0xFF

        def shift(value):
            return value >> 1

        def rot(value):
            return ((value << 3) | (value >> 5)) & 0xFF

        def absorb(value, round_no):
            return mix(trim(value), round_no)

        def squeeze(value):
            return rot(absorb(value, 7))

        def finish(value):
            return shift(squeeze(value))

        out = seed
        for round_no in range(8):
            out = absorb(out, round_no)
        return finish(out)

    def reset(self):
        return None
