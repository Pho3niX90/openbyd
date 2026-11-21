package io.github.pho3nix90.openbyd.testing;

import java.util.List;
import java.util.Map;
import java.util.Objects;
import org.jspecify.annotations.Nullable;

/** An account the fake server knows. */
public final class Account {
    private final String username;
    private final String password;
    private @Nullable String userId;
    private @Nullable List<String> guestVins;
    private @Nullable Boolean userTypeRule;

    /** An account with a made-up user ID that owns every vehicle it reaches. */
    public Account(String username, String password) {
        this.username = Objects.requireNonNull(username, "username");
        this.password = Objects.requireNonNull(password, "password");
    }

    /**
     * The account's user ID; made up when not set.
     *
     * @return this, to chain
     */
    public Account userId(String userId) {
        this.userId = Objects.requireNonNull(userId, "userId");
        return this;
    }

    /**
     * The VINs the account reaches as a guest.
     *
     * @return this, to chain
     */
    public Account guestVins(List<String> vins) {
        this.guestVins = List.copyOf(vins);
        return this;
    }

    /**
     * Whether the server insists on the {@code userType} a guest's requests carry. Default
     * {@code true}.
     *
     * @return this, to chain
     */
    public Account userTypeRule(boolean insists) {
        this.userTypeRule = insists;
        return this;
    }

    Map<String, Object> args() {
        return Calls.args(
                "username", username,
                "password", password,
                "user_id", userId,
                "guest_vins", guestVins,
                "user_type_rule", userTypeRule);
    }
}
