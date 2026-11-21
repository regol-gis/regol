import com.geodesk.feature.Features;
import com.geodesk.feature.Feature;

// Original, minimal adapter around the pinned Apache-2.0 Java SDK.
// Output is IDs only; no upstream implementation code is embedded here.
class java_oracle {
    public static void main(String[] args) {
        try (var library = Features.open(args[0])) {
            for (Feature f : library.select(args.length > 1 ? args[1] : "*")) {
                System.out.println((f.isNode() ? "N" : f.isWay() ? "W" : "R") + f.id());
            }
        }
    }
}
