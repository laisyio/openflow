import Darwin
import Foundation

/// What the process actually costs the system right now, in bytes.
///
/// `phys_footprint` out of `TASK_VM_INFO`, which is the number iOS itself uses
/// to decide who jetsam kills, and the one Xcode's memory gauge draws. Resident
/// set size is the obvious alternative and the wrong one: it counts pages that
/// are shared or file-backed and discounts compressed ones, so a model brought
/// in through mapped files can add hundreds of megabytes to what the phone will
/// charge us for while barely moving RSS.
///
/// PLAN.md section 5 asks for the memory figure to be measured and shown rather
/// than typed into the copy. This is the measurement.
enum ProcessFootprint {
    /// Nil when the kernel refuses, which it should never do for the calling
    /// task. A caller that gets nil reports the weights' size on disk instead
    /// and says so, rather than reporting zero.
    static func bytes() -> Int? {
        var info = task_vm_info_data_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size
        )
        let result = withUnsafeMutablePointer(to: &info) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { rebound in
                task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), rebound, &count)
            }
        }
        guard result == KERN_SUCCESS else { return nil }
        return Int(info.phys_footprint)
    }
}
