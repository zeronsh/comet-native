// Standalone graphics-driver footprint reproduction; no window or app data.
// clang -fobjc-arc -Wall -Wextra -framework Foundation -framework Metal \
//   -framework MetalPerformanceShaders scripts/macos-metal-memory-probe.m \
//   -o /tmp/metal-memory-probe
// /tmp/metal-memory-probe [blit|render|mixed|blur]
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <MetalPerformanceShaders/MetalPerformanceShaders.h>
#include <libproc.h>
#include <stdio.h>
#include <sys/resource.h>
#include <unistd.h>

int main(int argc, const char **argv) {
    @autoreleasepool {
        NSString *mode = argc > 1 ? @(argv[1]) : @"mixed";
        BOOL blit = [mode isEqualToString:@"blit"] || [mode isEqualToString:@"mixed"];
        BOOL render = ![mode isEqualToString:@"blit"];
        BOOL blur = [mode isEqualToString:@"blur"];
        if (![mode isEqualToString:@"blit"] && ![mode isEqualToString:@"render"] &&
            ![mode isEqualToString:@"mixed"] && !blur) {
            fprintf(stderr, "Usage: %s [blit|render|mixed|blur]\n", argv[0]);
            return 2;
        }
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        id<MTLCommandQueue> queue = [device newCommandQueue];
        if (!device || !queue) return 1;
        id<MTLBuffer> buffer = blit ? [device newBufferWithLength:1048576
            options:MTLResourceStorageModeShared] : nil;
        id<MTLTexture> texture = nil, output = nil;
        id<MTLRenderPipelineState> pipeline = nil;
        MPSImageGaussianBlur *gaussian = nil;
        if (render) {
            MTLTextureDescriptor *descriptor = [MTLTextureDescriptor
                texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                width:2640 height:1762 mipmapped:NO];
            descriptor.usage = MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead |
                MTLTextureUsageShaderWrite;
            descriptor.storageMode = MTLStorageModePrivate;
            texture = [device newTextureWithDescriptor:descriptor];
            NSError *error = nil;
            id<MTLLibrary> library = [device newLibraryWithSource:
                @"#include <metal_stdlib>\n"
                 "using namespace metal;\n"
                 "vertex float4 v(uint i [[vertex_id]]) {\n"
                 "float2 p[3]={float2(-1,-1),float2(3,-1),float2(-1,3)};\n"
                 "return float4(p[i],0,1); }\n"
                 "fragment float4 f(){return float4(0.2,0.3,0.4,1);}\n"
                options:nil error:&error];
            MTLRenderPipelineDescriptor *pipelineDescriptor = [MTLRenderPipelineDescriptor new];
            pipelineDescriptor.vertexFunction = [library newFunctionWithName:@"v"];
            pipelineDescriptor.fragmentFunction = [library newFunctionWithName:@"f"];
            pipelineDescriptor.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
            pipeline = [device newRenderPipelineStateWithDescriptor:pipelineDescriptor error:&error];
            if (!texture || !library || !pipeline) {
                fprintf(stderr, "Metal setup failed: %s\n", error.description.UTF8String);
                return 1;
            }
            if (blur) {
                output = [device newTextureWithDescriptor:descriptor];
                gaussian = [[MPSImageGaussianBlur alloc] initWithDevice:device sigma:8];
                if (!output || !gaussian) return 1;
            }
        }
        if (blit && !buffer) return 1;
        puts("sample,footprintMiB,metalAllocatedMiB");
        // Three isolated submissions, separated by 15 seconds of no GPU work.
        // Sample every 500 ms, including the driver's post-completion release.
        for (int sample = 0; sample < 80; sample++) {
            @autoreleasepool {
                if (sample % 30 == 0) {
                    id<MTLCommandBuffer> command = [queue commandBuffer];
                    if (blit) {
                        id<MTLBlitCommandEncoder> encoder = [command blitCommandEncoder];
                        [encoder fillBuffer:buffer range:NSMakeRange(0,1048576) value:1];
                        [encoder endEncoding];
                    }
                    if (render) {
                        MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
                        pass.colorAttachments[0].texture = texture;
                        pass.colorAttachments[0].loadAction = MTLLoadActionClear;
                        pass.colorAttachments[0].storeAction = MTLStoreActionStore;
                        id<MTLRenderCommandEncoder> encoder = [command renderCommandEncoderWithDescriptor:pass];
                        [encoder setRenderPipelineState:pipeline];
                        [encoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
                        [encoder endEncoding];
                    }
                    if (blur) [gaussian encodeToCommandBuffer:command
                        sourceTexture:texture destinationTexture:output];
                    [command commit];
                    [command waitUntilCompleted];
                    if (command.status == MTLCommandBufferStatusError) {
                        fprintf(stderr, "GPU command failed: %s\n", command.error.description.UTF8String);
                        return 1;
                    }
                }
                struct rusage_info_v4 usage = {0};
                if (proc_pid_rusage(getpid(), RUSAGE_INFO_V4, (rusage_info_t *)&usage)) return 1;
                printf("%d,%.3f,%.3f\n", sample, usage.ri_phys_footprint / 1048576.0,
                    device.currentAllocatedSize / 1048576.0);
                fflush(stdout);
            }
            usleep(500000);
        }
    }
    return 0;
}
