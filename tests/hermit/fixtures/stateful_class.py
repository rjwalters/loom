# Fixture: Class with state -- should NOT be flagged (existing behavior)
class Counter:
    def __init__(self):
        self.count = 0

    def increment(self):
        self.count += 1

    def get(self):
        return self.count
