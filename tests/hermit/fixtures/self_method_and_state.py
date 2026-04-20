# Fixture: Class with both self.method() calls AND self.x = (has state)
# Should NOT be flagged -- existing behavior, just verify no regression
class Processor:
    def __init__(self):
        self.results = []

    def process(self, data):
        result = self.transform(data)
        self.results.append(result)
        return result

    def transform(self, data):
        return data.upper()
