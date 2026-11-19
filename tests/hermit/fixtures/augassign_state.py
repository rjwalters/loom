# Fixture: State via augmented assignment -- should NOT be flagged (#13884)
# `self.count += 1` is ast.AugAssign; the old state test only matched
# ast.Assign, so this class was reported as stateless. The class attribute
# gives `self.count += 1` a value to read on first call.
class Tally:
    count = 0

    def bump(self):
        self.count += 1
