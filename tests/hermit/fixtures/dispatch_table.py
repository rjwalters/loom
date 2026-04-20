# Fixture: Dispatch-table class -- should NOT be flagged as stateless ceremony
# This class uses self.method() calls for internal dispatch and has no state.
class CommandRouter:
    """Routes commands to handler methods."""

    def route(self, command, args):
        handler = {
            "create": self.handle_create,
            "update": self.handle_update,
            "delete": self.handle_delete,
            "list": self.handle_list,
        }.get(command)
        if not handler:
            raise ValueError(f"Unknown command: {command}")
        return handler(args)

    def handle_create(self, args):
        return {"action": "create", "args": args}

    def handle_update(self, args):
        return {"action": "update", "args": args}

    def handle_delete(self, args):
        return {"action": "delete", "args": args}

    def handle_list(self, args):
        return {"action": "list", "args": args}
