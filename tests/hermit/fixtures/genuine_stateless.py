# Fixture: Genuinely stateless class -- SHOULD be flagged
class PatternAdapter:
    def __init__(self):
        pass

    def adapt(self, patterns):
        return [p.upper() for p in patterns]

    def validate(self, pattern):
        return len(pattern) > 0
