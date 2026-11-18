class UserRepository:
    def __init__(self):
        self.users = {}

    def find_user(self, user_id):
        """Look up a user by their unique identifier."""
        if user_id not in self.users:
            raise LookupError("USER_NOT_FOUND")
        return self.users[user_id]

    def save_user(self, user_id, profile):
        """Persist a user profile in the repository."""
        self.users[user_id] = profile


async def load_profile(repository, user_id):
    return repository.find_user(user_id)
