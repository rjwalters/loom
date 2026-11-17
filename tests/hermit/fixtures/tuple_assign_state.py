# Fixture: State via tuple assignment -- should NOT be flagged (#13884)
# `self.a, self.b = a, b` is ast.Assign whose target is ast.Tuple; the old
# state test only matched ast.Assign targets that are directly ast.Attribute,
# so this class was reported as stateless.
class PairHolder:
    def __init__(self, a, b):
        self.a, self.b = a, b

    def pair(self):
        return self.a, self.b
