package io.github.pho3nix90.openbyd.kotlin

import io.github.pho3nix90.openbyd.BydEndpointNotSupportedException
import io.github.pho3nix90.openbyd.Client
import io.github.pho3nix90.openbyd.ClientOptions
import io.github.pho3nix90.openbyd.GatePolicy
import io.github.pho3nix90.openbyd.OpenByd
import io.github.pho3nix90.openbyd.model.AcSwitch
import io.github.pho3nix90.openbyd.model.BydConfig
import io.github.pho3nix90.openbyd.model.CapabilitiesOptions
import io.github.pho3nix90.openbyd.model.EnergyType
import io.github.pho3nix90.openbyd.model.GpsInfo
import io.github.pho3nix90.openbyd.model.HvacStatus
import io.github.pho3nix90.openbyd.model.RequestRecord
import io.github.pho3nix90.openbyd.model.Vehicle
import io.github.pho3nix90.openbyd.testing.Account
import io.github.pho3nix90.openbyd.testing.FakeServer
import io.github.pho3nix90.openbyd.testing.Reply
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.ExecutionException
import java.util.concurrent.TimeUnit
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

/** The package from Kotlin: null-safe models, enums, callbacks as lambdas, futures awaited. */
class KotlinUsageTest {
    private val vin = "TESTVIN0000000001"

    @Test
    fun `a client reads typed, null-safe models`() {
        FakeServer(Account("user@example.com", "secret")).use { server ->
            server.reply("/app/account/getAllListByUserId", Reply.data(listOf(mapOf("vin" to vin, "energyType" to "0"))))
            server.reply("/control/getStatusNow", Reply.data(mapOf("acSwitch" to 2, "tempInCar" to 21)))
            server.reply("/control/smartCharge/homePage", Reply.code("1001"))

            // A callback is a lambda; it runs on the client's own thread.
            val requests = CopyOnWriteArrayList<RequestRecord>()
            val options = ClientOptions()
                .requestObserver { record -> requests.add(record) }
                .gatePolicy { _, verdict -> verdict.supported }
                .gatePolicy(GatePolicy.STRICT)
            val config = BydConfig("user@example.com", "secret").baseUrl(server.url()).mqttEnabled(false)

            Client(config, options).use { client ->
                // A future is awaited with get(); a coroutine would use kotlinx-coroutines-jdk8's await().
                client.login().get(30, TimeUnit.SECONDS)
                val vehicles: List<Vehicle> = client.getVehicles().get(30, TimeUnit.SECONDS)
                val vehicle = vehicles.single()
                // A component that is always there is not nullable, and an enum is an enum.
                val name: String = vehicle.vin
                assertEquals(vin, name)
                assertEquals(EnergyType.EV, vehicle.energyType)
                assertEquals(0, vehicle.energyType.value())

                // One that may be missing is nullable: the compiler asks for the check.
                val hvac: HvacStatus = client.getHvacStatus(vin).get(30, TimeUnit.SECONDS)
                val inside: Double? = hvac.tempInCar
                assertEquals(21.0, inside ?: Double.NaN)
                assertEquals(AcSwitch.ON, hvac.acSwitch)
                assertNull(hvac.mainSettingTemp)
                val warm = hvac.tempInCar?.let { it > 18 } ?: false
                assertTrue(warm)
                assertNull(client.vehicleMetadata("TESTVIN0000000009")?.modelName)

                // A failure is the future's: get() wraps it, and it is one of pyBYD's classes.
                val failed = assertThrows(ExecutionException::class.java) {
                    client.getChargingStatus(vin).get(30, TimeUnit.SECONDS)
                }
                val cause = failed.cause as BydEndpointNotSupportedException
                assertEquals("1001", cause.code())
                assertEquals("BydEndpointNotSupportedError", cause.kind())

                // Options chain, and a raw payload parses as a model by its class.
                val refresh = CapabilitiesOptions().forceRefresh(true)
                assertEquals(vin, client.getVehicleCapabilities(vin, refresh).get(30, TimeUnit.SECONDS).vin)
                val gps = OpenByd.parseModel(GpsInfo::class.java, mapOf("latitude" to 52.0, "longitude" to 4.0))
                assertEquals(52.0, gps.latitude)
                assertNull(gps.speed)

                // The observer heard the requests, a little after the calls that made them.
                val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10)
                while (requests.none { it.endpoint == "/control/getStatusNow" } && System.nanoTime() < deadline) {
                    Thread.sleep(5)
                }
                assertTrue(requests.any { it.endpoint == "/control/getStatusNow" && it.code == "0" })
            }
        }
    }
}
