using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

namespace OpenByd;

/// <summary>
/// The C library (<c>crates/openbyd-capi/include/openbyd.h</c>): five functions and two
/// callback types. Every string is UTF-8 and NUL-terminated; a string the library returns
/// is freed with <see cref="openbyd_string_free"/>.
/// </summary>
internal static unsafe partial class Native
{
    private const string Library = "openbyd_c";

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Utf8)]
    [UnmanagedCallConv(CallConvs = [typeof(CallConvCdecl)])]
    internal static partial byte* openbyd_call(string function, byte* args, out int ok);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Utf8)]
    [UnmanagedCallConv(CallConvs = [typeof(CallConvCdecl)])]
    internal static partial byte* openbyd_testing_call(string name, byte* args, out int ok);

    [LibraryImport(Library)]
    [UnmanagedCallConv(CallConvs = [typeof(CallConvCdecl)])]
    internal static partial nint openbyd_client_new(
        byte* config,
        byte* options,
        delegate* unmanaged[Cdecl]<void*, byte*, byte*, void> onEvent,
        delegate* unmanaged[Cdecl]<void*, ulong, int, byte*, void> onDone,
        void* userData,
        byte** error);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Utf8)]
    [UnmanagedCallConv(CallConvs = [typeof(CallConvCdecl)])]
    internal static partial byte* openbyd_client_call_sync(ClientHandle client, string method, byte* args, out int ok);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Utf8)]
    [UnmanagedCallConv(CallConvs = [typeof(CallConvCdecl)])]
    internal static partial void openbyd_client_call(ClientHandle client, ulong call, string method, byte* args);

    [LibraryImport(Library)]
    [UnmanagedCallConv(CallConvs = [typeof(CallConvCdecl)])]
    internal static partial void openbyd_client_free(nint client);

    [LibraryImport(Library)]
    [UnmanagedCallConv(CallConvs = [typeof(CallConvCdecl)])]
    internal static partial void openbyd_string_free(byte* text);
}

/// <summary>
/// A client of the C library, freed once: by <see cref="SafeHandle.Dispose()"/>, or by the
/// finalizer when the client was dropped without being disposed. A native call in progress
/// on another thread holds the handle open until it returns.
/// </summary>
internal sealed class ClientHandle : SafeHandle
{
    private readonly ClientCore _owner;
    private GCHandle _target;

    /// <param name="owner">Told when the native client is gone.</param>
    /// <param name="client">What <c>openbyd_client_new</c> returned.</param>
    /// <param name="target">The handle the native client was given as its <c>user_data</c>.</param>
    internal ClientHandle(ClientCore owner, nint client, GCHandle target)
        : base(0, ownsHandle: true)
    {
        _owner = owner;
        _target = target;
        SetHandle(client);
    }

    public override bool IsInvalid => handle == 0;

    protected override bool ReleaseHandle()
    {
        // No callback runs after this returns, so nothing reads the target any more.
        Native.openbyd_client_free(handle);
        _target.Free();
        _owner.Released();
        return true;
    }
}
