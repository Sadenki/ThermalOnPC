// Activates (or with "off", deactivates) the FlirProbe driver extension.
import Foundation
import SystemExtensions

let dextID = "org.flironepc.FlirProbe"

final class Delegate: NSObject, OSSystemExtensionRequestDelegate {
    func request(_ r: OSSystemExtensionRequest, actionForReplacingExtension old: OSSystemExtensionProperties,
                 withExtension new: OSSystemExtensionProperties) -> OSSystemExtensionRequest.ReplacementAction {
        print("replacing \(old.bundleVersion) with \(new.bundleVersion)")
        return .replace
    }
    func requestNeedsUserApproval(_ r: OSSystemExtensionRequest) {
        print("NEEDS APPROVAL: Réglages Système → Général → Éléments de connexion et extensions → Extensions de pilote")
    }
    func request(_ r: OSSystemExtensionRequest, didFinishWithResult result: OSSystemExtensionRequest.Result) {
        print("done: \(result == .completed ? "completed" : "will complete after reboot")")
        exit(0)
    }
    func request(_ r: OSSystemExtensionRequest, didFailWithError error: Error) {
        print("FAILED: \(error.localizedDescription) (\((error as NSError).code))")
        exit(1)
    }
}

let delegate = Delegate()
let off = CommandLine.arguments.dropFirst().first == "off"
let req = off
    ? OSSystemExtensionRequest.deactivationRequest(forExtensionWithIdentifier: dextID, queue: .main)
    : OSSystemExtensionRequest.activationRequest(forExtensionWithIdentifier: dextID, queue: .main)
req.delegate = delegate
OSSystemExtensionManager.shared.submitRequest(req)
print(off ? "deactivation submitted" : "activation submitted")
DispatchQueue.main.asyncAfter(deadline: .now() + 120) { print("timeout"); exit(2) }
dispatchMain()
