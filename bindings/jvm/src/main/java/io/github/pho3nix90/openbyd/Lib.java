package io.github.pho3nix90.openbyd;

import com.sun.jna.Callback;
import com.sun.jna.Library;
import com.sun.jna.Native;
import com.sun.jna.Platform;
import com.sun.jna.Pointer;
import com.sun.jna.ptr.IntByReference;
import com.sun.jna.ptr.PointerByReference;
import java.util.Map;
import org.jspecify.annotations.Nullable;

/**
 * openbyd's C library ({@code openbyd.h}), loaded through JNA.
 *
 * <p>JNA looks for {@code openbyd_c} in the directories of the system property {@code
 * jna.library.path}, then where the system keeps libraries, then in the jar under its platform's
 * prefix ({@code win32-x86-64/}, {@code linux-x86-64/}, {@code linux-aarch64/}, {@code
 * darwin-aarch64/}, {@code darwin-x86-64/}), which is how a released jar carries it.
 */
final class Lib {
    private static final String ENCODING = "UTF-8";

    /** The functions of {@code openbyd.h}. Every returned string is freed with {@link #openbyd_string_free}. */
    interface C extends Library {
        Pointer openbyd_call(String function, @Nullable String args, IntByReference ok);

        Pointer openbyd_testing_call(String name, @Nullable String args, IntByReference ok);

        @Nullable Pointer openbyd_client_new(
                String config,
                @Nullable String options,
                EventFn onEvent,
                DoneFn onDone,
                Pointer userData,
                PointerByReference error);

        Pointer openbyd_client_call_sync(Pointer client, String method, @Nullable String args, IntByReference ok);

        void openbyd_client_call(Pointer client, long call, String method, @Nullable String args);

        void openbyd_client_free(Pointer client);

        void openbyd_string_free(Pointer text);
    }

    /** {@code openbyd_event_fn}: an event's name and payload, valid until it returns. */
    interface EventFn extends Callback {
        void invoke(@Nullable Pointer userData, Pointer name, Pointer payload);
    }

    /** {@code openbyd_done_fn}: the outcome of a call, valid until it returns. */
    interface DoneFn extends Callback {
        void invoke(@Nullable Pointer userData, long call, int ok, Pointer json);
    }

    /** What a call answered: its result's JSON, or an error's. */
    record Outcome(boolean ok, String json) {}

    private static volatile @Nullable C library;

    private Lib() {}

    /** The library, loaded on first use. */
    static C c() {
        C loaded = library;
        if (loaded == null) {
            synchronized (Lib.class) {
                loaded = library;
                if (loaded == null) {
                    loaded = load();
                    library = loaded;
                }
            }
        }
        return loaded;
    }

    private static C load() {
        try {
            return Native.load("openbyd_c", C.class, Map.of(Library.OPTION_STRING_ENCODING, ENCODING));
        } catch (UnsatisfiedLinkError error) {
            UnsatisfiedLinkError clearer =
                    new UnsatisfiedLinkError(
                            "openbyd's native library (openbyd_c) was not found. Name its directory in the system"
                                    + " property jna.library.path, or use a jar that carries it under "
                                    + Platform.RESOURCE_PREFIX
                                    + "/. "
                                    + error.getMessage());
            clearer.initCause(error);
            throw clearer;
        }
    }

    /** The text of a string a callback was given. */
    static String text(Pointer text) {
        return text.getString(0, ENCODING);
    }

    /** Takes a string the library returned: its text, freed. */
    static String take(Pointer text) {
        try {
            return text.getString(0, ENCODING);
        } finally {
            c().openbyd_string_free(text);
        }
    }

    /** {@code openbyd_call}: a function that needs no client. */
    static Outcome call(String function, @Nullable String args) {
        IntByReference ok = new IntByReference(0);
        String json = take(c().openbyd_call(function, args, ok));
        return new Outcome(ok.getValue() != 0, json);
    }

    /** {@code openbyd_testing_call}: the fake server and the test broker. */
    static Outcome testingCall(String name, @Nullable String args) {
        IntByReference ok = new IntByReference(0);
        String json = take(c().openbyd_testing_call(name, args, ok));
        return new Outcome(ok.getValue() != 0, json);
    }

    /** {@code openbyd_client_call_sync}: a method that answers at once. */
    static Outcome callSync(Pointer client, String method, @Nullable String args) {
        IntByReference ok = new IntByReference(0);
        String json = take(c().openbyd_client_call_sync(client, method, args, ok));
        return new Outcome(ok.getValue() != 0, json);
    }
}
